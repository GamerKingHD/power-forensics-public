//! Range-focused forensic analysis over a recorded session.
//!
//! Everything here is pure, deterministic and rule-based: no ML, no LLM, no
//! IO. It answers "what does the recorded evidence say about this interval?"
//! and deliberately never upgrades an observation into a causal claim.
//!
//! All ranges are **wall-clock milliseconds** (the same domain the GUI charts
//! and selects in). Energy integration reuses [`crate::stats`] dual-clock
//! primitives so a range and the whole session can never disagree about a
//! discontinuity. Unavailable readings stay `None` and are never zero-filled.

use crate::analysis::MAX_CLOCK_SKEW_SECS;
use crate::session::SessionData;
use crate::stats;
use crate::telemetry::Telemetry;

/// A timestamped, provenance-carrying sample in the session wall-clock domain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub wall_ms: u64,
    pub value: Option<f64>,
    pub provenance: &'static str,
}

pub(crate) fn mono_wall(s: &SessionData, t: f64) -> u64 {
    s.wall_base_ms.saturating_add((t * 1000.0) as u64)
}

/// Wall time of a reading: its own wall stamp when present, else derived from
/// the session base plus the monotonic sample time. Never invents a zero.
pub(crate) fn wall_of(s: &SessionData, t: &Telemetry<f64>, mono_s: f64) -> u64 {
    if t.stamp.wall_millis > 0 {
        t.stamp.wall_millis
    } else {
        mono_wall(s, mono_s)
    }
}

fn sample(s: &SessionData, t: &Telemetry<f64>, mono_s: f64) -> Sample {
    Sample {
        wall_ms: wall_of(s, t, mono_s),
        value: t.val(),
        provenance: t.provenance().as_str(),
    }
}

// ---------------------------------------------------------------------------
// Descriptive statistics
// ---------------------------------------------------------------------------

/// Median/range summary of the known values in a window. `n` counts every
/// sample considered (including unavailable ones); `known` counts the ones
/// that carried a value, so coverage is explicit.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stat {
    pub n: usize,
    pub known: usize,
    pub median: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub p10: Option<f64>,
    pub p90: Option<f64>,
}

impl Stat {
    pub fn of(samples: &[Sample]) -> Stat {
        let vals: Vec<f64> = samples.iter().filter_map(|p| p.value).collect();
        if vals.is_empty() {
            return Stat {
                n: samples.len(),
                ..Default::default()
            };
        }
        let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
        let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mut sorted = vals.clone();
        Stat {
            n: samples.len(),
            known: vals.len(),
            median: stats::percentile_sorted(&mut sorted.clone(), 50.0),
            min: Some(min),
            max: Some(max),
            p10: stats::percentile_sorted(&mut sorted.clone(), 10.0),
            p90: stats::percentile_sorted(&mut sorted, 90.0),
        }
    }

    /// Fraction of considered samples that carried a value. `None` when the
    /// window had no samples at all (no coverage claim).
    pub fn coverage(&self) -> Option<f64> {
        if self.n == 0 {
            None
        } else {
            Some(self.known as f64 / self.n as f64)
        }
    }
}

fn stat_between(samples: &[Sample], from_ms: u64, to_ms: u64) -> Stat {
    let window: Vec<Sample> = samples
        .iter()
        .copied()
        .filter(|p| p.wall_ms >= from_ms && p.wall_ms <= to_ms)
        .collect();
    Stat::of(&window)
}

// ---------------------------------------------------------------------------
// Metric catalogue
// ---------------------------------------------------------------------------

/// One chartable/reportable metric and the samples that feed it. The key
/// matches the GUI window series key where a series exists.
#[derive(Debug, Clone)]
pub struct MetricSeries {
    pub domain: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub samples: Vec<Sample>,
}

/// Every metric the analysis layer understands, in a stable order. This is
/// deliberately a curated set (the analytically useful signals), not a dump
/// of every collector field.
pub fn metric_series(s: &SessionData) -> Vec<MetricSeries> {
    let mut out: Vec<MetricSeries> = Vec::new();
    let mut push = |domain: &'static str,
                    key: &'static str,
                    label: &'static str,
                    unit: &'static str,
                    samples: Vec<Sample>| {
        out.push(MetricSeries {
            domain,
            key,
            label,
            unit,
            samples,
        });
    };

    push(
        "power",
        "battery_discharge_w",
        "Battery discharge",
        "W",
        s.battery
            .iter()
            .map(|p| sample(s, &p.discharge, p.t))
            .collect(),
    );
    push(
        "power",
        "battery_charge_w",
        "Battery charge",
        "W",
        s.battery
            .iter()
            .map(|p| sample(s, &p.charge, p.t))
            .collect(),
    );
    push(
        "battery",
        "battery_pct",
        "Battery charge",
        "%",
        s.battery.iter().map(|p| sample(s, &p.pct, p.t)).collect(),
    );
    push(
        "cpu",
        "cpu_utility_pct",
        "CPU utility",
        "%",
        s.cpu.iter().map(|p| sample(s, &p.utility, p.t)).collect(),
    );
    push(
        "cpu",
        "cpu_pkg_power_w",
        "CPU package (measured)",
        "W",
        s.cpu
            .iter()
            .map(|p| sample(s, &p.pkg_power_w, p.t))
            .collect(),
    );
    push(
        "cpu",
        "cpu_pkg_derived_w",
        "CPU package (derived)",
        "W",
        s.cpu
            .iter()
            .map(|p| sample(s, &p.pkg_derived_w, p.t))
            .collect(),
    );
    push(
        "cpu",
        "cpu_c3_pct",
        "Processor C3",
        "%",
        s.cpu.iter().map(|p| sample(s, &p.c3_pct, p.t)).collect(),
    );
    push(
        "gpu",
        "gpu_util_pct",
        "GPU utilization",
        "%",
        s.gpu
            .iter()
            .map(|p| {
                let t = p.adapters.first().map(|a| &a.total_util);
                match t {
                    Some(t) => sample(s, t, p.t),
                    None => Sample {
                        wall_ms: mono_wall(s, p.t),
                        value: None,
                        provenance: "unavailable",
                    },
                }
            })
            .collect(),
    );
    push(
        "display",
        "display_brightness_pct",
        "Display brightness",
        "%",
        s.display
            .iter()
            .map(|p| sample(s, &p.brightness, p.t))
            .collect(),
    );
    push(
        "display",
        "display_refresh_hz",
        "Refresh rate",
        "Hz",
        s.display
            .iter()
            .map(|p| Sample {
                wall_ms: mono_wall(s, p.t),
                value: p.displays.first().and_then(|d| d.freq_hz).map(|x| x as f64),
                provenance: "measured",
            })
            .collect(),
    );
    push(
        "network",
        "net_rx_bps",
        "Network RX",
        "B/s",
        s.net.iter().map(|p| sample(s, &p.total_rx, p.t)).collect(),
    );
    push(
        "network",
        "net_tx_bps",
        "Network TX",
        "B/s",
        s.net.iter().map(|p| sample(s, &p.total_tx, p.t)).collect(),
    );
    push(
        "storage",
        "storage_disk_time_pct",
        "Disk activity",
        "%",
        s.storage
            .iter()
            .map(|p| sample(s, &p.disk_time, p.t))
            .collect(),
    );
    push(
        "storage",
        "storage_read_bps",
        "Disk read",
        "B/s",
        s.storage
            .iter()
            .map(|p| sample(s, &p.read_bps, p.t))
            .collect(),
    );
    push(
        "storage",
        "storage_write_bps",
        "Disk write",
        "B/s",
        s.storage
            .iter()
            .map(|p| sample(s, &p.write_bps, p.t))
            .collect(),
    );
    out
}

fn metric_of<'a>(series: &'a [MetricSeries], key: &str) -> Option<&'a MetricSeries> {
    series.iter().find(|m| m.key == key)
}

// ---------------------------------------------------------------------------
// Range quality + energy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RangeEnergy {
    pub discharge_wh: Option<f64>,
    pub charge_wh: Option<f64>,
    pub covered_s: f64,
    pub unknown_s: f64,
    pub unobserved_s: f64,
    pub discontinuities: usize,
    /// True when the interval contains a forced clock/sampling boundary, so
    /// any energy figure must be qualified.
    pub crosses_discontinuity: bool,
    /// Whether a discharge / charge series was actually present in the range.
    pub discharge_present: bool,
    pub charge_present: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RangeQuality {
    pub span_s: f64,
    /// Seconds of the span covered by known, contiguous discharge evidence.
    pub covered_s: f64,
    pub unknown_s: f64,
    pub unobserved_s: f64,
    pub coverage: Option<f64>,
    pub discontinuities: usize,
    pub timeouts: usize,
    pub recoveries: usize,
    pub errors: usize,
    pub events: usize,
    pub markers: usize,
    /// Battery samples considered in the interval (the primary series).
    pub samples: usize,
    /// Positive sample counts for every collector that reported in the range.
    pub collector_counts: Vec<(&'static str, usize)>,
    /// Collectors known to exist in the session with no samples in the range.
    pub stale_collectors: Vec<&'static str>,
}

fn collector_walls(s: &SessionData) -> Vec<(&'static str, Vec<u64>)> {
    let w = |t: &Telemetry<f64>, mono: f64| wall_of(s, t, mono);
    vec![
        (
            "battery",
            s.battery.iter().map(|p| w(&p.discharge, p.t)).collect(),
        ),
        ("cpu", s.cpu.iter().map(|p| w(&p.utility, p.t)).collect()),
        ("gpu", s.gpu.iter().map(|p| mono_wall(s, p.t)).collect()),
        (
            "display",
            s.display.iter().map(|p| w(&p.brightness, p.t)).collect(),
        ),
        ("net", s.net.iter().map(|p| w(&p.total_rx, p.t)).collect()),
        (
            "storage",
            s.storage.iter().map(|p| w(&p.disk_time, p.t)).collect(),
        ),
        ("proc", s.procs.iter().map(|p| mono_wall(s, p.t)).collect()),
        ("usb", s.usb.iter().map(|p| mono_wall(s, p.t)).collect()),
        (
            "self",
            s.selfmon.iter().map(|p| w(&p.cpu_pct, p.t)).collect(),
        ),
        (
            "os_power",
            s.policy_history
                .iter()
                .map(|(t, _)| mono_wall(s, *t))
                .collect(),
        ),
    ]
}

fn count_in_range(walls: &[u64], from_ms: u64, to_ms: u64) -> usize {
    walls
        .iter()
        .filter(|w| **w >= from_ms && **w <= to_ms)
        .count()
}

fn event_counts(s: &SessionData, from_ms: u64, to_ms: u64) -> (usize, usize, usize, usize, usize) {
    let mut events = 0;
    let mut markers = 0;
    let mut timeouts = 0;
    let mut recoveries = 0;
    let mut errors = 0;
    for e in &s.events {
        if e.wall_ms < from_ms || e.wall_ms > to_ms {
            continue;
        }
        events += 1;
        let k = e.kind.to_ascii_lowercase();
        if k == "marker" {
            markers += 1;
        } else if k.contains("timeout") {
            timeouts += 1;
        } else if k.contains("recover") {
            recoveries += 1;
        } else if k.contains("error") || k.contains("fail") {
            errors += 1;
        }
    }
    (events, markers, timeouts, recoveries, errors)
}

struct Clocks {
    times: Vec<f64>,
    walls: Vec<f64>,
    discharge: Vec<Option<f64>>,
    charge: Vec<Option<f64>>,
}

/// Battery evidence carrying both clocks, filtered to a range. Only samples
/// inside the interval are used; the pair spanning either boundary is never
/// bridged, so partial intervals stay honestly unknown.
fn battery_clocks(s: &SessionData, from_ms: u64, to_ms: u64) -> Clocks {
    let mut c = Clocks {
        times: Vec::new(),
        walls: Vec::new(),
        discharge: Vec::new(),
        charge: Vec::new(),
    };
    for p in &s.battery {
        let wall = wall_of(s, &p.discharge, p.t);
        if wall < from_ms || wall > to_ms {
            continue;
        }
        c.times.push(p.t);
        c.walls.push(p.discharge.stamp.wall_millis as f64 / 1000.0);
        c.discharge.push(p.discharge.val());
        c.charge.push(p.charge.val());
    }
    c
}

pub fn range_energy(s: &SessionData, interval_ms: u64, from_ms: u64, to_ms: u64) -> RangeEnergy {
    let c = battery_clocks(s, from_ms, to_ms);
    let gap = stats::energy_max_gap_secs(interval_ms);
    let dseg = stats::integrate_segmented_clocks(
        &c.times,
        &c.walls,
        &c.discharge,
        gap,
        MAX_CLOCK_SKEW_SECS,
    );
    let cseg =
        stats::integrate_segmented_clocks(&c.times, &c.walls, &c.charge, gap, MAX_CLOCK_SKEW_SECS);
    RangeEnergy {
        discharge_wh: (dseg.covered_secs > 0.0).then_some(dseg.energy_wh),
        charge_wh: (cseg.covered_secs > 0.0).then_some(cseg.energy_wh),
        covered_s: dseg.covered_secs,
        unknown_s: dseg.unknown_secs,
        unobserved_s: dseg.unobserved_secs,
        discontinuities: dseg.discontinuities,
        crosses_discontinuity: dseg.discontinuities > 0,
        discharge_present: c.discharge.iter().any(|v| v.is_some()),
        charge_present: c.charge.iter().any(|v| v.is_some()),
    }
}

pub fn range_quality(s: &SessionData, interval_ms: u64, from_ms: u64, to_ms: u64) -> RangeQuality {
    let energy = range_energy(s, interval_ms, from_ms, to_ms);
    let span_s = if to_ms > from_ms {
        (to_ms - from_ms) as f64 / 1000.0
    } else {
        0.0
    };
    let mut collector_counts: Vec<(&'static str, usize)> = Vec::new();
    let mut stale_collectors: Vec<&'static str> = Vec::new();
    for (name, walls) in collector_walls(s) {
        if walls.is_empty() {
            // The collector never recorded anything session-wide: report it as
            // absent (the GUI distinguishes this from "stale in this range").
            continue;
        }
        let n = count_in_range(&walls, from_ms, to_ms);
        if n == 0 {
            stale_collectors.push(name);
        } else {
            collector_counts.push((name, n));
        }
    }
    let samples = count_in_range(
        &s.battery
            .iter()
            .map(|p| wall_of(s, &p.discharge, p.t))
            .collect::<Vec<_>>(),
        from_ms,
        to_ms,
    );
    let (events, markers, timeouts, recoveries, errors) = event_counts(s, from_ms, to_ms);
    let coverage_denom = energy.covered_s + energy.unknown_s;
    RangeQuality {
        span_s,
        covered_s: energy.covered_s,
        unknown_s: energy.unknown_s,
        unobserved_s: energy.unobserved_s,
        coverage: (energy.covered_s > 0.0 && coverage_denom > 0.0)
            .then_some(energy.covered_s / coverage_denom),
        discontinuities: energy.discontinuities,
        timeouts,
        recoveries,
        errors,
        events,
        markers,
        samples,
        collector_counts,
        stale_collectors,
    }
}

// ---------------------------------------------------------------------------
// Domain breakdown
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct MetricStat {
    pub key: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub stat: Stat,
    pub provenance: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DomainStat {
    pub domain: &'static str,
    pub metrics: Vec<MetricStat>,
    /// True when at least one metric carried a known value in the range.
    pub available: bool,
}

fn dominant_provenance(samples: &[Sample]) -> &'static str {
    let mut measured = 0;
    let mut derived = 0;
    let mut estimated = 0;
    for p in samples {
        match p.provenance {
            "measured" => measured += 1,
            "derived" => derived += 1,
            "estimated" => estimated += 1,
            _ => {}
        }
    }
    if measured >= derived && measured >= estimated && measured > 0 {
        "measured"
    } else if derived >= estimated && derived > 0 {
        "derived"
    } else if estimated > 0 {
        "estimated"
    } else {
        "unavailable"
    }
}

pub fn domain_breakdown(s: &SessionData, from_ms: u64, to_ms: u64) -> Vec<DomainStat> {
    let series = metric_series(s);
    let domains = [
        "power", "battery", "cpu", "gpu", "display", "network", "storage",
    ];
    let mut out = Vec::new();
    for domain in domains {
        let mut metrics = Vec::new();
        for m in series.iter().filter(|m| m.domain == domain) {
            let window: Vec<Sample> = m
                .samples
                .iter()
                .copied()
                .filter(|p| p.wall_ms >= from_ms && p.wall_ms <= to_ms)
                .collect();
            let stat = Stat::of(&window);
            metrics.push(MetricStat {
                key: m.key,
                label: m.label,
                unit: m.unit,
                provenance: dominant_provenance(&window),
                stat,
            });
        }
        let available = metrics.iter().any(|m| m.stat.known > 0);
        out.push(DomainStat {
            domain,
            metrics,
            available,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Before / during / after + change facts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WindowStats {
    pub before: Stat,
    pub during: Stat,
    pub after: Stat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeFact {
    pub domain: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    /// Reference (before, or after when before is missing) and during medians.
    pub reference: Option<f64>,
    pub during: Option<f64>,
    pub after: Option<f64>,
    pub delta: Option<f64>,
    /// "increased" | "decreased" | "appeared" | "disappeared"
    pub direction: &'static str,
    /// "high" | "medium" | "low"
    pub confidence: &'static str,
    /// Observational wording; never a causal claim.
    pub basis: &'static str,
    pub n_before: usize,
    pub n_during: usize,
    pub n_after: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CategoryChange {
    pub domain: &'static str,
    pub label: &'static str,
    pub before: Option<String>,
    pub during: Option<String>,
    pub after: Option<String>,
    pub basis: &'static str,
}

/// Window edges derived from the selected interval: adjacent before/after
/// windows of the same length, bounded by the session and a sensible maximum.
pub fn context_windows(s: &SessionData, from_ms: u64, to_ms: u64) -> (u64, u64, u64, u64) {
    let dur = to_ms.saturating_sub(from_ms).max(1);
    let ctx = dur.clamp(10_000, 5 * 60_000);
    let session_start = s.wall_base_ms;
    let session_end = session_end_wall(s).unwrap_or(to_ms);
    let before_start = from_ms.saturating_sub(ctx).max(session_start);
    let after_end = to_ms.saturating_add(ctx).min(session_end.max(to_ms));
    (before_start, from_ms, to_ms, after_end)
}

fn stat_in(samples: &[Sample], from_ms: u64, to_ms: u64) -> Stat {
    stat_between(samples, from_ms, to_ms)
}

pub fn windowed_metrics(s: &SessionData, from_ms: u64, to_ms: u64) -> Vec<(String, WindowStats)> {
    let series = metric_series(s);
    let (b0, b1, a0, a1) = context_windows(s, from_ms, to_ms);
    series
        .iter()
        .map(|m| {
            (
                m.key.to_string(),
                WindowStats {
                    before: stat_in(&m.samples, b0, b1),
                    during: stat_in(&m.samples, b1, a0),
                    after: stat_in(&m.samples, a0, a1),
                },
            )
        })
        .collect()
}

struct ChangeDef {
    key: &'static str,
    threshold_abs: f64,
    threshold_rel: Option<f64>,
}

fn change_defs() -> Vec<ChangeDef> {
    vec![
        ChangeDef {
            key: "battery_discharge_w",
            threshold_abs: 0.5,
            threshold_rel: Some(0.10),
        },
        ChangeDef {
            key: "cpu_utility_pct",
            threshold_abs: 5.0,
            threshold_rel: None,
        },
        ChangeDef {
            key: "cpu_pkg_power_w",
            threshold_abs: 0.5,
            threshold_rel: None,
        },
        ChangeDef {
            key: "cpu_pkg_derived_w",
            threshold_abs: 0.5,
            threshold_rel: None,
        },
        ChangeDef {
            key: "cpu_c3_pct",
            threshold_abs: 10.0,
            threshold_rel: None,
        },
        ChangeDef {
            key: "gpu_util_pct",
            threshold_abs: 5.0,
            threshold_rel: None,
        },
        ChangeDef {
            key: "display_brightness_pct",
            threshold_abs: 5.0,
            threshold_rel: None,
        },
        ChangeDef {
            key: "net_rx_bps",
            threshold_abs: 64.0 * 1024.0,
            threshold_rel: Some(0.5),
        },
        ChangeDef {
            key: "net_tx_bps",
            threshold_abs: 64.0 * 1024.0,
            threshold_rel: Some(0.5),
        },
        ChangeDef {
            key: "storage_disk_time_pct",
            threshold_abs: 5.0,
            threshold_rel: None,
        },
        ChangeDef {
            key: "storage_read_bps",
            threshold_abs: 256.0 * 1024.0,
            threshold_rel: Some(0.5),
        },
        ChangeDef {
            key: "storage_write_bps",
            threshold_abs: 256.0 * 1024.0,
            threshold_rel: Some(0.5),
        },
    ]
}

fn basis_for(domain: &str) -> &'static str {
    match domain {
        "power" => "Observed alongside the selected interval",
        "cpu" => "CPU activity changed during this interval",
        "gpu" => "GPU activity changed during this interval",
        "display" => "Display state changed during this interval",
        "network" => "Network activity changed during this interval",
        "storage" => "Storage activity changed during this interval",
        "battery" => "Battery state changed during this interval",
        _ => "Changed during this interval",
    }
}

fn classify_change(m: &MetricSeries, def: &ChangeDef, w: &WindowStats) -> Option<ChangeFact> {
    // Without any before/after context (e.g. the whole session is selected)
    // there is nothing to compare against: never claim an "appeared".
    if w.before.n == 0 && w.after.n == 0 {
        return None;
    }
    let reference = w.before.median.or(w.after.median);
    let during = w.during.median;
    let (reference, delta, direction) = match (reference, during) {
        (Some(r), Some(d)) => {
            let delta = d - r;
            let mut threshold = def.threshold_abs;
            if let Some(rel) = def.threshold_rel {
                threshold = threshold.max(r.abs() * rel);
            }
            if delta.abs() < threshold {
                return None;
            }
            (
                Some(r),
                Some(delta),
                if delta > 0.0 {
                    "increased"
                } else {
                    "decreased"
                },
            )
        }
        (None, Some(_)) => {
            // Was unobserved before the interval but is known inside it.
            (None, None, "appeared")
        }
        _ => return None,
    };
    // Guard against claiming a change from a couple of samples.
    let n_before = w.before.known;
    let n_during = w.during.known;
    if direction == "appeared" {
        if n_during < 3 {
            return None;
        }
    } else if n_before < 3 || n_during < 3 {
        return None;
    }
    let confidence = if direction == "appeared" {
        if n_during >= 5 { "medium" } else { "low" }
    } else {
        let delta = delta.unwrap_or(0.0);
        let mut threshold = def.threshold_abs;
        if let Some(rel) = def.threshold_rel {
            threshold = threshold.max(reference.unwrap_or(0.0).abs() * rel);
        }
        if delta.abs() >= 2.0 * threshold && n_before >= 5 && n_during >= 5 {
            "high"
        } else if n_before >= 3 && n_during >= 3 {
            "medium"
        } else {
            "low"
        }
    };
    Some(ChangeFact {
        domain: m.domain,
        key: m.key,
        label: m.label,
        unit: m.unit,
        reference,
        during,
        after: w.after.median,
        delta,
        direction,
        confidence,
        basis: basis_for(m.domain),
        n_before: w.before.known,
        n_during,
        n_after: w.after.known,
    })
}

pub fn detect_changes(s: &SessionData, from_ms: u64, to_ms: u64) -> Vec<ChangeFact> {
    let series = metric_series(s);
    let windowed = windowed_metrics(s, from_ms, to_ms);
    let defs = change_defs();
    let mut out = Vec::new();
    for def in &defs {
        let Some(m) = metric_of(&series, def.key) else {
            continue;
        };
        let Some(w) = windowed.iter().find(|(k, _)| k == def.key).map(|(_, w)| w) else {
            continue;
        };
        if let Some(fact) = classify_change(m, def, w) {
            out.push(fact);
        }
    }
    // Power first: it is the analytical anchor. Then by magnitude/threshold.
    out.sort_by(|a, b| {
        let ap = a.domain == "power";
        let bp = b.domain == "power";
        bp.cmp(&ap)
            .then_with(|| b.confidence.cmp(a.confidence))
            .then_with(|| a.label.cmp(b.label))
    });
    out
}

// ---------------------------------------------------------------------------
// Categorical context (scheme, AC, top process, refresh)
// ---------------------------------------------------------------------------

pub(crate) fn policy_last_window(s: &SessionData, from_ms: u64, to_ms: u64) -> Option<String> {
    s.policy_history
        .iter()
        .rfind(|(t, _)| {
            let w = mono_wall(s, *t);
            w >= from_ms && w <= to_ms
        })
        .and_then(|(_, p)| p.scheme_name.clone())
}

pub(crate) fn ac_last_window(s: &SessionData, from_ms: u64, to_ms: u64) -> Option<String> {
    s.battery_extra
        .iter()
        .rfind(|p| {
            let w = wall_of(s, &p.ac, p.t);
            w >= from_ms && w <= to_ms
        })
        .and_then(|p| p.ac.val())
        .map(|v| if v >= 0.5 { "AC" } else { "battery" }.to_string())
}

fn top_process_last_window(s: &SessionData, from_ms: u64, to_ms: u64) -> Option<String> {
    s.procs
        .iter()
        .rfind(|p| {
            let w = mono_wall(s, p.t);
            w >= from_ms && w <= to_ms
        })
        .and_then(|p| p.top.first())
        .map(|e| format!("{} (pid {})", e.name, e.pid))
}

pub(crate) fn refresh_last_window(s: &SessionData, from_ms: u64, to_ms: u64) -> Option<String> {
    s.display
        .iter()
        .rfind(|p| {
            let w = mono_wall(s, p.t);
            w >= from_ms && w <= to_ms
        })
        .and_then(|p| p.displays.first())
        .and_then(|d| d.freq_hz)
        .map(|hz| format!("{hz} Hz"))
}

pub fn categorical_changes(s: &SessionData, from_ms: u64, to_ms: u64) -> Vec<CategoryChange> {
    let (b0, b1, a0, a1) = context_windows(s, from_ms, to_ms);
    let mut out = Vec::new();
    let mut push_simple =
        |domain: &'static str, label: &'static str, pick: &dyn Fn(u64, u64) -> Option<String>| {
            let before = pick(b0, b1);
            let during = pick(b1, a0);
            let after = pick(a0, a1);
            let changed = (before.is_some() || during.is_some()) && before != during;
            if changed || (after != during && after.is_some()) {
                out.push(CategoryChange {
                    domain,
                    label,
                    before,
                    during,
                    after,
                    basis: basis_for(domain),
                });
            }
        };
    push_simple("power", "Power scheme", &|f, t| policy_last_window(s, f, t));
    push_simple("battery", "Power source", &|f, t| ac_last_window(s, f, t));
    push_simple("cpu", "Top-CPU process", &|f, t| {
        top_process_last_window(s, f, t)
    });
    push_simple("display", "Refresh rate", &|f, t| {
        refresh_last_window(s, f, t)
    });
    out
}

// ---------------------------------------------------------------------------
// Process evidence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub name: String,
    pub ppid: u32,
    pub cpu_median_pct: Option<f64>,
    pub cpu_max_pct: Option<f64>,
    pub presence: usize,
    pub ticks: usize,
    pub first_seen_ms: Option<u64>,
    pub last_seen_ms: Option<u64>,
    pub start_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessAnalysis {
    pub ticks: usize,
    pub entries: Vec<ProcessEntry>,
    pub total_threads: Option<u64>,
    pub inaccessible: Option<usize>,
    pub truncated: Option<bool>,
    /// True when any observed tick was incomplete; None when no coverage
    /// metadata exists (unknown, never "complete").
    pub incomplete: Option<bool>,
    pub note: String,
}

pub fn process_analysis(s: &SessionData, from_ms: u64, to_ms: u64) -> ProcessAnalysis {
    use std::collections::HashMap;
    let ticks: Vec<&crate::session::ProcPoint> = s
        .procs
        .iter()
        .filter(|p| {
            let w = mono_wall(s, p.t);
            w >= from_ms && w <= to_ms
        })
        .collect();
    let mut out = ProcessAnalysis {
        ticks: ticks.len(),
        ..Default::default()
    };
    if ticks.is_empty() {
        out.note = "No process snapshot falls inside the selected interval.".to_string();
        return out;
    }
    struct Agg {
        name: String,
        ppid: u32,
        start_unix_ms: Option<u64>,
        cpu: Vec<f64>,
        presence: usize,
        first: Option<u64>,
        last: Option<u64>,
    }
    let mut map: HashMap<u32, Agg> = HashMap::new();
    let mut total_threads: Option<u64> = None;
    let mut inaccessible: Option<usize> = None;
    let mut truncated: Option<bool> = None;
    for p in &ticks {
        if let Some(t) = p.total_threads {
            total_threads = Some(t);
        }
        if let Some(i) = p.inaccessible {
            inaccessible = Some(inaccessible.map_or(i, |prev| prev.max(i)));
        }
        if let Some(t) = p.truncated {
            truncated = Some(truncated.unwrap_or(false) || t);
        }
        let wall = mono_wall(s, p.t);
        for e in &p.top {
            let agg = map.entry(e.pid).or_insert_with(|| Agg {
                name: e.name.clone(),
                ppid: e.ppid,
                start_unix_ms: e.start_unix_ms,
                cpu: Vec::new(),
                presence: 0,
                first: None,
                last: None,
            });
            agg.presence += 1;
            if let Some(v) = e.cpu.val() {
                agg.cpu.push(v);
            }
            agg.first = Some(agg.first.map_or(wall, |f| f.min(wall)));
            agg.last = Some(agg.last.map_or(wall, |l| l.max(wall)));
        }
    }
    let mut entries: Vec<ProcessEntry> = map
        .into_iter()
        .map(|(pid, a)| ProcessEntry {
            pid,
            name: a.name,
            ppid: a.ppid,
            cpu_median_pct: stats::median(&a.cpu),
            cpu_max_pct: a.cpu.iter().copied().reduce(f64::max),
            presence: a.presence,
            ticks: ticks.len(),
            first_seen_ms: a.first,
            last_seen_ms: a.last,
            start_unix_ms: a.start_unix_ms,
        })
        .collect();
    entries.sort_by(|a, b| {
        b.cpu_median_pct
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&a.cpu_median_pct.unwrap_or(f64::NEG_INFINITY))
            .then_with(|| a.pid.cmp(&b.pid))
    });

    let incomplete = {
        let mut any_known = false;
        let mut incomplete = false;
        for p in &ticks {
            if p.inaccessible.is_none() && p.truncated.is_none() {
                continue;
            }
            any_known = true;
            if p.inaccessible.unwrap_or(0) > 0 || p.truncated.unwrap_or(false) {
                incomplete = true;
            }
        }
        any_known.then_some(incomplete)
    };
    let note = match incomplete {
        Some(true) => "Process evidence incomplete for this interval.".to_string(),
        Some(false) => "Process enumeration reported complete for this interval.".to_string(),
        None => "Process enumeration completeness was not recorded for this interval.".to_string(),
    };
    out.entries = entries;
    out.total_threads = total_threads;
    out.inaccessible = inaccessible;
    out.truncated = truncated;
    out.incomplete = incomplete;
    out.note = note;
    out
}

// ---------------------------------------------------------------------------
// Correlation (explicitly not causation)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Correlation {
    pub x: &'static str,
    pub y: &'static str,
    pub label: String,
    pub n: usize,
    pub effective_n: usize,
    pub r: Option<f64>,
    /// Aligned pairs / the smaller known sample count.
    pub coverage: f64,
    /// "insufficient" | "negligible" | "weak" | "moderate" | "strong"
    pub strength: &'static str,
    pub note: String,
}

/// Minimum aligned samples before a coefficient is reported at all.
pub const MIN_CORRELATION_SAMPLES: usize = 20;
const MIN_CORRELATION_COVERAGE: f64 = 0.5;
const MAX_CADENCE_RATIO: f64 = 3.0;

fn pearson(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() != y.len() || x.len() < 3 {
        return None;
    }
    let n = x.len() as f64;
    let mx = x.iter().sum::<f64>() / n;
    let my = y.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut dx = 0.0;
    let mut dy = 0.0;
    for i in 0..x.len() {
        let a = x[i] - mx;
        let b = y[i] - my;
        num += a * b;
        dx += a * a;
        dy += b * b;
    }
    if dx <= 0.0 || dy <= 0.0 {
        return None;
    }
    Some((num / (dx.sqrt() * dy.sqrt())).clamp(-1.0, 1.0))
}

fn unique_count(vals: &[f64]) -> usize {
    let mut seen: Vec<i64> = vals.iter().map(|v| (v * 100.0).round() as i64).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

fn median_dt_ms(samples: &[Sample]) -> f64 {
    let ts: Vec<f64> = samples
        .iter()
        .filter(|p| p.value.is_some())
        .map(|p| p.wall_ms as f64)
        .collect();
    if ts.len() < 2 {
        return 0.0;
    }
    let mut d: Vec<f64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    d.sort_by(|a, b| a.total_cmp(b));
    d[d.len() / 2]
}

fn align(x: &[Sample], y: &[Sample], tol_ms: u64) -> Vec<(f64, f64)> {
    let xs: Vec<&Sample> = x.iter().filter(|p| p.value.is_some()).collect();
    let ys: Vec<&Sample> = y.iter().filter(|p| p.value.is_some()).collect();
    let mut out = Vec::new();
    let mut j = 0usize;
    for p in xs {
        while j + 1 < ys.len() && ys[j + 1].wall_ms <= p.wall_ms {
            j += 1;
        }
        let mut best: Option<&Sample> = None;
        for k in [j.saturating_sub(1), j] {
            if let Some(c) = ys.get(k) {
                let d = c.wall_ms.abs_diff(p.wall_ms);
                if d <= tol_ms
                    && best
                        .map(|b| d < b.wall_ms.abs_diff(p.wall_ms))
                        .unwrap_or(true)
                {
                    best = Some(c);
                }
            }
        }
        if let Some(b) = best {
            out.push((p.value.unwrap(), b.value.unwrap()));
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn correlation_pair(
    series: &[MetricSeries],
    x_key: &'static str,
    y_key: &'static str,
    label: &str,
    from_ms: u64,
    to_ms: u64,
    tol_ms: u64,
    discontinuities: usize,
) -> Correlation {
    let empty = |note: String| Correlation {
        x: x_key,
        y: y_key,
        label: label.to_string(),
        n: 0,
        effective_n: 0,
        r: None,
        coverage: 0.0,
        strength: "insufficient",
        note,
    };
    let (Some(x), Some(y)) = (metric_of(series, x_key), metric_of(series, y_key)) else {
        return empty("metric not present".to_string());
    };
    let xw: Vec<Sample> = x
        .samples
        .iter()
        .copied()
        .filter(|p| p.wall_ms >= from_ms && p.wall_ms <= to_ms)
        .collect();
    let yw: Vec<Sample> = y
        .samples
        .iter()
        .copied()
        .filter(|p| p.wall_ms >= from_ms && p.wall_ms <= to_ms)
        .collect();
    let known_x = xw.iter().filter(|p| p.value.is_some()).count();
    let known_y = yw.iter().filter(|p| p.value.is_some()).count();
    if known_x < 3 || known_y < 3 {
        return empty("insufficient evidence: no aligned samples in the interval".to_string());
    }
    let cadence_x = median_dt_ms(&xw);
    let cadence_y = median_dt_ms(&yw);
    if cadence_x > 0.0 && cadence_y > 0.0 {
        let ratio = (cadence_x / cadence_y).max(cadence_y / cadence_x);
        if ratio > MAX_CADENCE_RATIO {
            return empty(format!(
                "insufficient evidence: collector cadence differs by {:.1}x",
                ratio
            ));
        }
    }
    let pairs = align(&xw, &yw, tol_ms);
    let n = pairs.len();
    let denom = known_x.min(known_y).max(1);
    let coverage = n as f64 / denom as f64;
    if discontinuities > 0 {
        return Correlation {
            n,
            effective_n: 0,
            coverage,
            r: None,
            ..empty("insufficient evidence: interval crosses a clock discontinuity".to_string())
        };
    }
    if n < MIN_CORRELATION_SAMPLES {
        let mut c = empty(format!(
            "insufficient evidence: only {n} aligned samples (need {MIN_CORRELATION_SAMPLES})"
        ));
        c.n = n;
        c.coverage = coverage;
        return c;
    }
    if coverage < MIN_CORRELATION_COVERAGE {
        let mut c = empty(format!(
            "insufficient evidence: only {:.0}% of samples align",
            coverage * 100.0
        ));
        c.n = n;
        c.coverage = coverage;
        return c;
    }
    let xv: Vec<f64> = pairs.iter().map(|p| p.0).collect();
    let yv: Vec<f64> = pairs.iter().map(|p| p.1).collect();
    let r = pearson(&xv, &yv);
    let unique = unique_count(&xv).min(unique_count(&yv));
    let rho = stats::lag1_autocorr(&xv)
        .abs()
        .max(stats::lag1_autocorr(&yv).abs());
    let effective = stats::effective_n(n, unique, rho);
    let strength = match r.map(f64::abs) {
        Some(v) if v >= 0.7 => "strong",
        Some(v) if v >= 0.4 => "moderate",
        Some(v) if v >= 0.2 => "weak",
        Some(_) => "negligible",
        None => "insufficient",
    };
    let note = match strength {
        "negligible" => "No meaningful linear relationship in this interval.",
        "insufficient" => "Insufficient variance to estimate a relationship.",
        _ => "Correlation only; it does not establish that one metric caused the other.",
    };
    Correlation {
        x: x_key,
        y: y_key,
        label: label.to_string(),
        n,
        effective_n: effective,
        r,
        coverage,
        strength,
        note: note.to_string(),
    }
}

/// Correlations that are statistically defensible for this range. Every pair
/// returns a result, including an explicit "insufficient evidence" one.
pub fn correlations(
    s: &SessionData,
    interval_ms: u64,
    from_ms: u64,
    to_ms: u64,
) -> Vec<Correlation> {
    let series = metric_series(s);
    let tol = {
        let base = metric_of(&series, "battery_discharge_w")
            .map(|m| median_dt_ms(&m.samples))
            .unwrap_or(1000.0);
        (base * 1.5).max(2000.0) as u64
    };
    // A discontinuity inside the interval makes a single coefficient
    // indefensible: the two clocks disagree about elapsed time, so every
    // pair is reported as insufficient rather than a spliced number.
    let range_has_disc = range_energy(s, interval_ms, from_ms, to_ms).discontinuities > 0;
    let pairs: [(&'static str, &'static str, &str); 5] = [
        (
            "battery_discharge_w",
            "cpu_utility_pct",
            "Power vs CPU utility",
        ),
        (
            "battery_discharge_w",
            "gpu_util_pct",
            "Power vs GPU utility",
        ),
        (
            "battery_discharge_w",
            "display_brightness_pct",
            "Power vs display brightness",
        ),
        ("battery_discharge_w", "net_rx_bps", "Power vs network RX"),
        (
            "battery_discharge_w",
            "storage_disk_time_pct",
            "Power vs disk activity",
        ),
    ];
    pairs
        .iter()
        .map(|(x, y, label)| {
            correlation_pair(
                &series,
                x,
                y,
                label,
                from_ms,
                to_ms,
                tol,
                usize::from(range_has_disc),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Interesting regions (deterministic, explainable)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct InterestingRegion {
    /// "peak" | "sustained_increase" | "marker" | "discontinuity"
    pub kind: &'static str,
    pub domain: &'static str,
    pub label: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub score: f64,
    pub detail: String,
}

struct Bucket {
    start_ms: u64,
    end_ms: u64,
    median: Option<f64>,
    max: Option<f64>,
    known: usize,
}

fn bucketize(samples: &[Sample], start_ms: u64, end_ms: u64, bucket_ms: u64) -> Vec<Bucket> {
    if end_ms <= start_ms || bucket_ms == 0 {
        return Vec::new();
    }
    let count = (end_ms - start_ms).div_ceil(bucket_ms).max(1) as usize;
    let mut buckets: Vec<Bucket> = (0..count)
        .map(|i| Bucket {
            start_ms: start_ms + i as u64 * bucket_ms,
            end_ms: (start_ms + (i as u64 + 1) * bucket_ms).min(end_ms),
            median: None,
            max: None,
            known: 0,
        })
        .collect();
    let mut vals: Vec<Vec<f64>> = vec![Vec::new(); count];
    for p in samples {
        if p.wall_ms < start_ms || p.wall_ms >= end_ms {
            continue;
        }
        let i = ((p.wall_ms - start_ms) / bucket_ms) as usize;
        if i >= count {
            continue;
        }
        if let Some(v) = p.value {
            vals[i].push(v);
        }
    }
    for i in 0..count {
        if vals[i].is_empty() {
            continue;
        }
        let v = &vals[i];
        buckets[i].known = v.len();
        buckets[i].median = stats::percentile_sorted(&mut v.clone(), 50.0);
        buckets[i].max = v.iter().copied().reduce(f64::max);
    }
    buckets
}

fn session_end_wall(s: &SessionData) -> Option<u64> {
    let mut last = 0u64;
    let mut any = false;
    for p in &s.battery {
        last = last.max(wall_of(s, &p.discharge, p.t));
        any = true;
    }
    for p in &s.cpu {
        last = last.max(wall_of(s, &p.utility, p.t));
        any = true;
    }
    for p in &s.procs {
        last = last.max(mono_wall(s, p.t));
        any = true;
    }
    for p in &s.display {
        last = last.max(wall_of(s, &p.brightness, p.t));
        any = true;
    }
    for p in &s.net {
        last = last.max(wall_of(s, &p.total_rx, p.t));
        any = true;
    }
    for p in &s.storage {
        last = last.max(wall_of(s, &p.disk_time, p.t));
        any = true;
    }
    if any { Some(last) } else { None }
}

pub fn session_span_ms(s: &SessionData) -> (u64, u64) {
    let start = s.wall_base_ms;
    let end = session_end_wall(s).unwrap_or(start);
    (start, end.max(start))
}

/// Candidate interesting regions for the whole session. Deterministic and
/// explainable: peaks, sustained rises, markers and discontinuities. These
/// are candidates, not anomalies — no anomaly model exists.
pub fn interesting_regions(s: &SessionData, interval_ms: u64) -> Vec<InterestingRegion> {
    let (start_ms, end_ms) = session_span_ms(s);
    let mut out: Vec<InterestingRegion> = Vec::new();
    let span = end_ms.saturating_sub(start_ms);
    if span == 0 {
        return out;
    }
    let bucket_ms = (span / 60).max(30_000);
    let series = metric_series(s);

    let mut scan = |key: &'static str,
                    domain: &'static str,
                    peak_label: &str,
                    rise_threshold: f64,
                    rel_threshold: f64| {
        let Some(m) = metric_of(&series, key) else {
            return;
        };
        let known: Vec<f64> = m.samples.iter().filter_map(|p| p.value).collect();
        let Some(overall) = stats::median(&known) else {
            return;
        };
        let buckets = bucketize(&m.samples, start_ms, end_ms, bucket_ms);
        // Peaks: highest bucket maxima above the session median.
        let mut peaks: Vec<&Bucket> = buckets.iter().filter(|b| b.known >= 2).collect();
        peaks.sort_by(|a, b| {
            b.max
                .unwrap_or(f64::NEG_INFINITY)
                .total_cmp(&a.max.unwrap_or(f64::NEG_INFINITY))
        });
        for b in peaks.into_iter().take(4) {
            let max = b.max.unwrap_or(0.0);
            let delta = max - overall;
            if delta >= rise_threshold
                && (overall.abs() < 1e-9 || delta / overall.abs() >= rel_threshold)
            {
                out.push(InterestingRegion {
                    kind: "peak",
                    domain,
                    label: peak_label.to_string(),
                    start_ms: b.start_ms,
                    end_ms: b.end_ms,
                    score: max,
                    detail: format!(
                        "peak {max:.2} vs session median {overall:.2} over this bucket"
                    ),
                });
            }
        }
        // Sustained increases between adjacent buckets.
        let mut rises: Vec<(usize, f64)> = Vec::new();
        for i in 1..buckets.len() {
            if let (Some(a), Some(b)) = (buckets[i - 1].median, buckets[i].median) {
                let d = b - a;
                if d >= rise_threshold && (a.abs() < 1e-9 || d / a.abs() >= rel_threshold) {
                    rises.push((i, d));
                }
            }
        }
        rises.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (i, d) in rises.into_iter().take(2) {
            out.push(InterestingRegion {
                kind: "sustained_increase",
                domain,
                label: format!("{peak_label} rise"),
                start_ms: buckets[i - 1].start_ms,
                end_ms: buckets[i].end_ms,
                score: d,
                detail: format!("sustained increase of {d:.2} between adjacent buckets"),
            });
        }
    };

    scan("battery_discharge_w", "power", "Power peak", 1.0, 0.25);
    scan("cpu_utility_pct", "cpu", "CPU activity peak", 10.0, 0.2);
    scan("gpu_util_pct", "gpu", "GPU activity peak", 5.0, 0.2);

    // Markers: exact positions, never averaged.
    for e in &s.events {
        if e.kind.eq_ignore_ascii_case("marker") {
            out.push(InterestingRegion {
                kind: "marker",
                domain: "events",
                label: format!("Marker: {}", e.detail),
                start_ms: e.wall_ms,
                end_ms: e.wall_ms,
                score: 50.0,
                detail: format!("user marker at {}", e.wall_ms),
            });
        }
    }

    // Discontinuities carry their own gravity: they bound trustworthy energy.
    let c = battery_clocks(s, start_ms, end_ms);
    let gap = stats::energy_max_gap_secs(interval_ms);
    for d in stats::discontinuities(&c.times, &c.walls, gap, MAX_CLOCK_SKEW_SECS) {
        if d.index + 1 >= c.times.len() {
            continue;
        }
        let at = c.times[d.index + 1] * 1000.0;
        let wall = start_ms.saturating_add(at as u64);
        out.push(InterestingRegion {
            kind: "discontinuity",
            domain: "power",
            label: format!("Clock discontinuity ({})", d.kind.as_str()),
            start_ms: wall.saturating_sub(1000),
            end_ms: wall.saturating_add(1000),
            score: d.unobserved_secs.max(1.0),
            detail: format!(
                "{} clock step; {:.0}s not integrated",
                d.kind.as_str(),
                d.unobserved_secs
            ),
        });
    }

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.start_ms.cmp(&b.start_ms))
    });
    out.truncate(24);
    out
}

// ---------------------------------------------------------------------------
// Top-level range analysis
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct RangeAnalysis {
    pub from_ms: u64,
    pub to_ms: u64,
    pub duration_s: f64,
    pub quality: RangeQuality,
    pub energy: RangeEnergy,
    pub domains: Vec<DomainStat>,
    pub processes: ProcessAnalysis,
    pub changes: Vec<ChangeFact>,
    pub categorical: Vec<CategoryChange>,
    pub correlations: Vec<Correlation>,
}

pub fn analyze_range(s: &SessionData, interval_ms: u64, from_ms: u64, to_ms: u64) -> RangeAnalysis {
    let (from_ms, to_ms) = (from_ms.min(to_ms), to_ms.max(from_ms));
    RangeAnalysis {
        from_ms,
        to_ms,
        duration_s: (to_ms - from_ms) as f64 / 1000.0,
        quality: range_quality(s, interval_ms, from_ms, to_ms),
        energy: range_energy(s, interval_ms, from_ms, to_ms),
        domains: domain_breakdown(s, from_ms, to_ms),
        processes: process_analysis(s, from_ms, to_ms),
        changes: detect_changes(s, from_ms, to_ms),
        categorical: categorical_changes(s, from_ms, to_ms),
        correlations: correlations(s, interval_ms, from_ms, to_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{BatteryPoint, CpuPoint, ProcEntry, ProcPoint, SessionData};
    use crate::telemetry::{ClockStamp, Telemetry};

    fn stamp(ms: u64) -> ClockStamp {
        ClockStamp {
            wall_millis: ms,
            mono_millis: ms,
        }
    }

    /// Steady 6 W discharge for 60 samples at 1 s, wall base 1000.
    fn steady() -> SessionData {
        let mut s = SessionData {
            label: "t".to_string(),
            wall_base_ms: 1000,
            ..Default::default()
        };
        for i in 0..60 {
            s.battery.push(BatteryPoint {
                t: i as f64,
                discharge: Telemetry::measured(6.0, "battery", stamp(1000 + i * 1000)),
                ..Default::default()
            });
        }
        s
    }

    #[test]
    fn full_range_energy_matches_whole_session_primitive() {
        let s = steady();
        let (start, end) = session_span_ms(&s);
        let e = range_energy(&s, 1000, start, end);
        // 60 samples over 59 s at 6 W => 0.0983 Wh.
        assert!((e.discharge_wh.unwrap() - 6.0 * 59.0 / 3600.0).abs() < 1e-9);
        assert!((e.covered_s - 59.0).abs() < 1e-9);
        assert_eq!(e.discontinuities, 0);
        assert!(!e.crosses_discontinuity);
    }

    #[test]
    fn range_energy_excludes_partial_and_gap_intervals() {
        let mut s = steady();
        // Punch a 30 s hole at t=20..50 (samples removed).
        s.battery.retain(|p| !(20.0..50.0).contains(&p.t));
        let (start, end) = session_span_ms(&s);
        let e = range_energy(&s, 1000, start, end);
        assert!(e.unobserved_s > 25.0, "gap must be unobserved: {e:?}");
        assert_eq!(e.discontinuities, 1);
        assert!(e.crosses_discontinuity);
    }

    #[test]
    fn unavailable_discharge_stays_unknown_not_zero() {
        let mut s = steady();
        for p in s.battery.iter_mut() {
            p.discharge = Telemetry::unavailable(
                crate::telemetry::UnavailKind::NotSampled,
                "on ac",
                "battery",
                stamp(1000),
            );
        }
        let (start, end) = session_span_ms(&s);
        let e = range_energy(&s, 1000, start, end);
        assert_eq!(e.discharge_wh, None);
        assert!(!e.discharge_present);
        let q = range_quality(&s, 1000, start, end);
        assert_eq!(q.coverage, None, "no known span means no coverage claim");
    }

    #[test]
    fn before_during_after_and_changes() {
        let mut s = steady();
        // CPU jumps from 5% to 40% at t=30.
        for i in 0..60 {
            let u = if i < 30 { 5.0 } else { 40.0 };
            s.cpu.push(CpuPoint {
                t: i as f64,
                utility: Telemetry::measured(u, "cpu", stamp(1000 + i * 1000)),
                ..Default::default()
            });
        }
        let from = 1000 + 30_000;
        let to = 1000 + 45_000;
        let (b0, b1, a0, a1) = context_windows(&s, from, to);
        assert!(b0 < b1 && b1 == from && a0 == to && a1 > a0);
        let w = windowed_metrics(&s, from, to);
        let cpu = w.iter().find(|(k, _)| k == "cpu_utility_pct").unwrap();
        assert!((cpu.1.before.median.unwrap() - 5.0).abs() < 1e-9);
        assert!((cpu.1.during.median.unwrap() - 40.0).abs() < 1e-9);
        let changes = detect_changes(&s, from, to);
        let fact = changes
            .iter()
            .find(|c| c.key == "cpu_utility_pct")
            .expect("cpu change detected");
        assert_eq!(fact.direction, "increased");
        assert!(fact.delta.unwrap() > 0.0);
        // Observational wording, never causal.
        assert!(!fact.basis.to_lowercase().contains("caused"));
    }

    #[test]
    fn full_session_selection_has_no_change_context() {
        let mut s = steady();
        for i in 0..60 {
            let u = if i < 30 { 5.0 } else { 40.0 };
            s.cpu.push(CpuPoint {
                t: i as f64,
                utility: Telemetry::measured(u, "cpu", stamp(1000 + i * 1000)),
                ..Default::default()
            });
        }
        let (start, end) = session_span_ms(&s);
        // Selecting the whole session leaves no before/after window, so no
        // change can honestly be claimed.
        assert!(detect_changes(&s, start, end).is_empty());
    }

    #[test]
    fn process_coverage_flags_incompleteness() {
        let mut s = steady();
        s.procs.push(ProcPoint {
            t: 0.0,
            inaccessible: Some(12),
            truncated: Some(true),
            total_threads: Some(2200),
            top: vec![ProcEntry {
                pid: 42,
                name: "game.exe".to_string(),
                cpu: Telemetry::measured(80.0, "proc", stamp(1000)),
                ..Default::default()
            }],
            ..Default::default()
        });
        let p = process_analysis(&s, 1000, 60_000);
        assert_eq!(p.incomplete, Some(true));
        assert_eq!(p.inaccessible, Some(12));
        assert_eq!(p.truncated, Some(true));
        assert_eq!(p.total_threads, Some(2200));
        assert_eq!(p.entries.len(), 1);
        assert!(p.note.to_lowercase().contains("incomplete"));
    }

    #[test]
    fn process_unknown_completeness_is_not_complete() {
        let mut s = steady();
        s.procs.push(ProcPoint {
            t: 0.0,
            top: vec![ProcEntry {
                pid: 1,
                name: "x".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let p = process_analysis(&s, 1000, 60_000);
        assert_eq!(p.incomplete, None, "missing metadata must be unknown");
    }

    #[test]
    fn correlation_reports_insufficient_for_tiny_samples() {
        let s = steady(); // only 60 battery samples, no other metric
        let cs = correlations(&s, 1000, 1000, 60_000);
        assert!(!cs.is_empty());
        assert!(cs.iter().all(|c| c.strength == "insufficient"));
        assert!(cs.iter().any(|c| c.note.contains("insufficient")));
    }

    #[test]
    fn correlation_detects_a_strong_relationship() {
        let mut s = SessionData {
            label: "c".to_string(),
            wall_base_ms: 0,
            ..Default::default()
        };
        for i in 0..80 {
            let load = (i % 20) as f64; // 0..19 %
            s.battery.push(BatteryPoint {
                t: i as f64,
                discharge: Telemetry::measured(5.0 + load * 0.25, "battery", stamp(i * 1000)),
                ..Default::default()
            });
            s.cpu.push(CpuPoint {
                t: i as f64,
                utility: Telemetry::measured(load, "cpu", stamp(i * 1000)),
                ..Default::default()
            });
        }
        let cs = correlations(&s, 1000, 0, 80_000);
        let power_cpu = cs
            .iter()
            .find(|c| c.x == "battery_discharge_w" && c.y == "cpu_utility_pct")
            .unwrap();
        assert!(power_cpu.r.unwrap() > 0.99, "{power_cpu:?}");
        assert_eq!(power_cpu.strength, "strong");
        assert!(power_cpu.effective_n >= 1);
    }

    #[test]
    fn interesting_regions_find_a_peak_and_a_gap() {
        let mut s = SessionData {
            label: "r".to_string(),
            wall_base_ms: 0,
            ..Default::default()
        };
        for i in 0..120 {
            // Baseline 6 W, with a 20 W spike at t=60.
            let w = if i == 60 { 20.0 } else { 6.0 };
            s.battery.push(BatteryPoint {
                t: i as f64,
                discharge: Telemetry::measured(w, "battery", stamp(i * 1000)),
                ..Default::default()
            });
        }
        // Remove a block to create a discontinuity.
        s.battery.retain(|p| !(90.0..110.0).contains(&p.t));
        let regions = interesting_regions(&s, 1000);
        assert!(regions.iter().any(|r| r.kind == "peak"), "{regions:?}");
        assert!(
            regions.iter().any(|r| r.kind == "discontinuity"),
            "{regions:?}"
        );
    }

    #[test]
    fn categorical_detects_scheme_change() {
        let mut s = steady();
        let a = crate::session::Policy {
            scheme_name: Some("Balanced".to_string()),
            ..Default::default()
        };
        let b = crate::session::Policy {
            scheme_name: Some("Power saver".to_string()),
            ..Default::default()
        };
        s.policy_history.push((20.0, a));
        s.policy_history.push((38.0, b));
        let changes = categorical_changes(&s, 1000 + 30_000, 1000 + 45_000);
        let scheme = changes.iter().find(|c| c.label == "Power scheme").unwrap();
        assert_eq!(scheme.before.as_deref(), Some("Balanced"));
        assert_eq!(scheme.during.as_deref(), Some("Power saver"));
    }
}
