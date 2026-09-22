//! Analysis engine foundation: summaries, baselines, step detection.
//!
//! Pure functions only. Rule-based and deterministic; no ML, no LLM.
//! Every attribution carries the evidence and a confidence level.

use crate::stats;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

/// Median/p10/p90 summary of a power series, e.g. an idle baseline.
#[derive(Debug, Clone)]
pub struct PowerSummary {
    pub n: usize,
    pub median: f64,
    pub p10: f64,
    pub p90: f64,
    pub mean: f64,
}

impl PowerSummary {
    pub fn of(values: &[f64]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        Some(PowerSummary {
            n: values.len(),
            median: stats::percentile_sorted(&mut sorted.clone(), 50.0)?,
            p10: stats::percentile_sorted(&mut sorted.clone(), 10.0)?,
            p90: stats::percentile_sorted(&mut sorted, 90.0)?,
            mean: stats::mean(values)?,
        })
    }

    /// Deviation of this summary from a baseline: (abs_watts, rel_fraction).
    pub fn deviation_from(&self, baseline: &PowerSummary) -> (f64, f64) {
        let abs = self.median - baseline.median;
        let rel = if baseline.median.abs() > f64::EPSILON {
            abs / baseline.median
        } else {
            f64::NAN
        };
        (abs, rel)
    }
}

/// Unattributed system power: battery discharge minus reliably measured
/// components. The remainder is explicitly unknown, never zero-filled.
pub fn unattributed_power(discharge_w: f64, measured_components_w: &[f64]) -> f64 {
    discharge_w - measured_components_w.iter().sum::<f64>()
}

/// A detected step change in system power with supporting evidence.
#[derive(Debug, Clone)]
pub struct StepChange {
    pub index: usize,
    pub before_median_w: f64,
    pub after_median_w: f64,
    pub delta_w: f64,
    pub confidence: Confidence,
    pub evidence: String,
}

/// Scan a power series for the single largest step between consecutive
/// windows of `window` samples. Returns None when no step clears
/// `threshold_w`. Deterministic and O(n).
pub fn detect_step(power: &[f64], window: usize, threshold_w: f64) -> Option<StepChange> {
    if window == 0 || power.len() < 2 * window {
        return None;
    }
    let mut best: Option<StepChange> = None;
    for i in window..=(power.len() - window) {
        let before = PowerSummary::of(&power[i - window..i])?;
        let after = PowerSummary::of(&power[i..i + window])?;
        let delta = after.median - before.median;
        let dominated = best
            .as_ref()
            .map(|b: &StepChange| delta.abs() > b.delta_w.abs());
        if delta.abs() >= threshold_w && dominated.unwrap_or(true) {
            let confidence = if delta.abs() >= 2.0 * threshold_w {
                Confidence::High
            } else {
                Confidence::Medium
            };
            best = Some(StepChange {
                index: i,
                before_median_w: before.median,
                after_median_w: after.median,
                delta_w: delta,
                confidence,
                evidence: format!(
                    "median {:.2} W -> {:.2} W over {}-sample windows at index {}",
                    before.median, after.median, window, i
                ),
            });
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_of_idle_baseline() {
        let s = PowerSummary::of(&[5.4, 5.1, 5.9, 5.6, 5.8]).unwrap();
        assert_eq!(s.n, 5);
        assert!((s.median - 5.6).abs() < 1e-9);
        let (abs, rel) = s.deviation_from(&PowerSummary::of(&[5.4, 5.1]).unwrap());
        assert!(abs > 0.0 && rel > 0.0);
    }

    #[test]
    fn summary_rejects_empty() {
        assert!(PowerSummary::of(&[]).is_none());
    }

    #[test]
    fn unattributed_is_explicit_remainder() {
        // 6.4 W discharge, 1.1 CPU + 0.4 GPU + 0.8 known = 4.1 unattributed.
        assert!((unattributed_power(6.4, &[1.1, 0.4, 0.8]) - 4.1).abs() < 1e-9);
        // No component collectors yet: entire discharge is unattributed.
        assert!((unattributed_power(6.4, &[]) - 6.4).abs() < 1e-9);
    }

    #[test]
    fn detects_spec_step_example() {
        // 5.7 W baseline, then a jump to ~11.4 W.
        let mut power = vec![5.7; 20];
        power.extend(vec![11.4; 20]);
        // Add slight noise so medians stay honest.
        power[5] = 5.9;
        power[25] = 11.2;
        let step = detect_step(&power, 10, 1.0).unwrap();
        assert!((step.delta_w - 5.7).abs() < 0.3);
        assert_eq!(step.confidence, Confidence::High);
    }

    #[test]
    fn no_step_below_threshold() {
        let power = vec![5.5; 40];
        assert!(detect_step(&power, 10, 1.0).is_none());
        assert!(detect_step(&power, 0, 1.0).is_none());
        assert!(detect_step(&[1.0], 10, 1.0).is_none());
    }
}

// ============================ analysis engine ============================
// Rule-based baselines, step attribution, A/B comparison, and reporting.
// Deterministic; no ML, no LLM. Correlations are labeled as such — the
// engine never upgrades correlation to proven causation.

use crate::json::JVal;
use crate::session::{ProcPoint, SessionData};

#[derive(Debug, Clone)]
pub struct Evidence {
    pub label: String,
    pub detail: String,
    /// Watt delta, if the evidence is actually in watts. Engine-% and
    /// utilization deltas live in `detail` text only — never here.
    pub delta_w: Option<f64>,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Default)]
pub struct Diagnosis {
    pub discharge_n: usize,
    pub median_w: Option<f64>,
    pub baseline_abs_w: Option<f64>,
    pub baseline_rel: Option<f64>,
    pub step: Option<StepChange>,
    pub evidence: Vec<Evidence>,
    pub unattributed_w: Option<f64>,
    /// Set when there is nothing to diagnose (e.g. on AC the whole time).
    pub note: String,
}

/// Median of series values with t in [t0, t1). Index-agnostic so series
/// of different lengths (dropped collector samples) still align.
fn time_median(series: &[(f64, f64)], t0: f64, t1: f64) -> Option<f64> {
    let vals: Vec<f64> = series
        .iter()
        .filter(|(t, _)| *t >= t0 && *t < t1)
        .map(|(_, v)| *v)
        .collect();
    stats::median(&vals)
}

fn median_interval(ts: &[f64]) -> f64 {
    if ts.len() < 2 {
        return 1.0;
    }
    let mut diffs: Vec<f64> = ts.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    diffs.sort_by(|a, b| a.total_cmp(b));
    diffs[diffs.len() / 2].max(0.05)
}

/// Process lifecycle across consecutive top-N snapshots. True exit needs
/// kernel tracing (ETW, privileged), so stops are estimated: an identity
/// absent for >3 consecutive ticks is recorded as stopped at last-seen +
/// one interval (see `proc::exit_hint`), labeled estimated. Starts are
/// identities first seen after the opening tick.
#[derive(Debug, Clone)]
pub struct ProcStart {
    pub pid: u32,
    pub name: String,
    pub first_seen_t: f64,
}

#[derive(Debug, Clone)]
pub struct ProcStop {
    pub pid: u32,
    pub name: String,
    pub last_seen_t: f64,
    pub exit_est_t: f64,
    pub missed_ticks: usize,
}

pub fn lifecycle_across_ticks(snaps: &[ProcPoint]) -> (Vec<ProcStart>, Vec<ProcStop>) {
    use std::collections::HashMap;
    #[derive(Clone)]
    struct Span {
        name: String,
        first: usize,
        last: usize,
    }
    let mut spans: HashMap<(u32, Option<u64>), Span> = HashMap::new();
    for (i, snap) in snaps.iter().enumerate() {
        for e in &snap.top {
            spans
                .entry((e.pid, e.start_unix_ms))
                .and_modify(|sp| sp.last = i)
                .or_insert(Span {
                    name: e.name.clone(),
                    first: i,
                    last: i,
                });
        }
    }
    let ts: Vec<f64> = snaps.iter().map(|p| p.t).collect();
    let dt = median_interval(&ts);
    let n = snaps.len();
    let mut starts = Vec::new();
    let mut stops = Vec::new();
    let mut keys: Vec<(u32, Option<u64>)> = spans.keys().cloned().collect();
    keys.sort();
    for k in keys {
        let sp = &spans[&k];
        if sp.first > 0 {
            starts.push(ProcStart {
                pid: k.0,
                name: sp.name.clone(),
                first_seen_t: snaps[sp.first].t,
            });
        }
        let missed = n.saturating_sub(sp.last + 1);
        if let Some(exit) = exit_hint(snaps[sp.last].t, dt, missed) {
            stops.push(ProcStop {
                pid: k.0,
                name: sp.name.clone(),
                last_seen_t: snaps[sp.last].t,
                exit_est_t: exit,
                missed_ticks: missed,
            });
        }
    }
    (starts, stops)
}

/// GPU power-sensor status as diagnosis evidence: unavailable with the
/// per-backend reasons, never fake watts.
fn gpu_sensor_evidence() -> Evidence {
    let reasons: Vec<String> = available_adapters()
        .iter()
        .map(|a| format!("{}: {}", a.name, a.limitations))
        .collect();
    Evidence {
        label: "GPU power sensor".to_string(),
        detail: format!("unavailable ({})", reasons.join("; ")),
        delta_w: None,
        confidence: Confidence::Low,
    }
}

pub fn diagnose_session(s: &SessionData, baseline_median: Option<f64>) -> Diagnosis {
    let dis: Vec<(f64, f64)> = s
        .battery
        .iter()
        .filter_map(|p| p.discharge.val().map(|w| (p.t, w)))
        .collect();
    let mut d = Diagnosis {
        discharge_n: dis.len(),
        ..Default::default()
    };
    if dis.is_empty() {
        d.note = "no battery discharge samples (on AC or charging throughout)".to_string();
        return d;
    }
    let vals: Vec<f64> = dis.iter().map(|(_, w)| *w).collect();
    let summary = match PowerSummary::of(&vals) {
        Some(x) => x,
        None => {
            d.note = "empty discharge series".to_string();
            return d;
        }
    };
    d.median_w = Some(summary.median);
    if let Some(b) = baseline_median
        && b > 0.0
    {
        d.baseline_abs_w = Some(summary.median - b);
        d.baseline_rel = Some((summary.median - b) / b);
    }
    let mut ev: Vec<Evidence> = Vec::new();
    ev.push(gpu_sensor_evidence());
    let w = (dis.len() / 10).clamp(5, 60);
    let step = match detect_step(&vals, w, 1.0) {
        Some(st) => st,
        None => {
            d.note = "no step change >= 1.0 W found; discharge looks stable".to_string();
            d.evidence = ev;
            return d;
        }
    };
    let t_step = dis[step.index].0;
    let ts: Vec<f64> = dis.iter().map(|(t, _)| *t).collect();
    let dt = median_interval(&ts);
    let (b0, b1, a1) = (t_step - w as f64 * dt, t_step, t_step + w as f64 * dt);

    let cpu_u: Vec<(f64, f64)> = s
        .cpu
        .iter()
        .filter_map(|p| p.utility.val().map(|v| (p.t, v)))
        .collect();
    let cpu_c3: Vec<(f64, f64)> = s
        .cpu
        .iter()
        .filter_map(|p| p.c3_pct.val().map(|v| (p.t, v)))
        .collect();
    let cpu_pkg: Vec<(f64, f64)> = s
        .cpu
        .iter()
        .filter_map(|p| p.pkg_derived_w.val().map(|v| (p.t, v)))
        .collect();
    let bri: Vec<(f64, f64)> = s
        .display
        .iter()
        .filter_map(|p| p.brightness.val().map(|v| (p.t, v)))
        .collect();

    // GPU activity per adapter (engine-%, never watts).
    let mut names: Vec<String> = Vec::new();
    for g in &s.gpu {
        for a in &g.adapters {
            if !names.contains(&a.name) {
                names.push(a.name.clone());
            }
        }
    }
    for name in &names {
        let series: Vec<(f64, f64)> = s
            .gpu
            .iter()
            .filter_map(|g| {
                g.adapters
                    .iter()
                    .find(|a| &a.name == name)
                    .and_then(|a| a.total_util.val().map(|u| (g.t, u)))
            })
            .collect();
        if let (Some(before), Some(after)) = (
            time_median(&series, b0, b1),
            time_median(&series, t_step, a1),
        ) {
            let delta = after - before;
            if delta.abs() >= ACTIVE_UTIL_PCT {
                ev.push(Evidence {
                    label: format!("GPU activity ({name})"),
                    detail: format!("engine sum {before:.1}% -> {after:.1}% within one window"),
                    delta_w: None,
                    confidence: if after > ACTIVE_UTIL_PCT {
                        Confidence::High
                    } else {
                        Confidence::Medium
                    },
                });
            }
        }
    }
    // CPU utilization shift (context, not watts).
    if let (Some(before), Some(after)) =
        (time_median(&cpu_u, b0, b1), time_median(&cpu_u, t_step, a1))
        && (after - before).abs() >= 3.0
    {
        ev.push(Evidence {
            label: "CPU utilization shift".to_string(),
            detail: format!("utility {before:.1}% -> {after:.1}%"),
            delta_w: None,
            confidence: Confidence::Medium,
        });
    }
    // Processor C3 residency drop. This is processor-package C3, not a
    // measured package-deep-idle residency, so the label states only what
    // the `% C3 Time` counter supports.
    if let (Some(before), Some(after)) = (
        time_median(&cpu_c3, b0, b1),
        time_median(&cpu_c3, t_step, a1),
    ) && before - after >= 10.0
    {
        ev.push(Evidence {
                label: "Processor C3 residency decreased".to_string(),
                detail: format!(
                    "processor % C3 Time {before:.0}% -> {after:.0}%; may correlate with higher idle power"
                ),
                delta_w: None,
                confidence: Confidence::High,
            });
    }
    // Package power is estimated: watts with low confidence, kept out of
    // the unattributed remainder math.
    if let (Some(before), Some(after)) = (
        time_median(&cpu_pkg, b0, b1),
        time_median(&cpu_pkg, t_step, a1),
    ) && (after - before).abs() >= 0.5
    {
        ev.push(Evidence {
            label: "CPU package power (estimated)".to_string(),
            detail: format!(
                "Energy-Meter derivation {before:.1} W -> {after:.1} W; model, not sensor"
            ),
            delta_w: Some(after - before),
            confidence: Confidence::Low,
        });
    }
    // Display brightness.
    if let (Some(before), Some(after)) = (time_median(&bri, b0, b1), time_median(&bri, t_step, a1))
        && (after - before).abs() >= 5.0
    {
        ev.push(Evidence {
            label: "Display brightness change".to_string(),
            detail: format!("sensor {before:.0}% -> {after:.0}% (no display power sensor exists)"),
            delta_w: None,
            confidence: Confidence::Medium,
        });
    }
    // Process lifecycle across the step. Only a start time inside the
    // after-window is evidence of creation; a process that was already
    // running is reported as becoming active or entering the top-15 list,
    // never as "new". Absence from a top-N list is not proof of exit, so
    // stops are not claimed from this data.
    struct Seen {
        ppid: u32,
        name: String,
        cpu: Option<f64>,
        start_unix_ms: Option<u64>,
    }
    let procs_in = |t0: f64, t1: f64| -> std::collections::HashMap<u32, Seen> {
        let mut m = std::collections::HashMap::new();
        for p in &s.procs {
            if p.t >= t0 && p.t < t1 {
                for e in &p.top {
                    m.entry(e.pid).or_insert_with(|| Seen {
                        ppid: e.ppid,
                        name: e.name.clone(),
                        cpu: e.cpu.val(),
                        start_unix_ms: e.start_unix_ms,
                    });
                }
            }
        }
        m
    };
    let before_p = procs_in(b0, b1);
    let after_p = procs_in(t_step, a1);
    let wall_at = |t: f64| s.wall_base_ms as f64 + t * 1000.0;
    let mut arrivals: Vec<(u32, &Seen)> = after_p
        .iter()
        .filter(|(pid, _)| !before_p.contains_key(pid))
        .map(|(pid, e)| (*pid, e))
        .collect();
    arrivals.sort_by_key(|(pid, _)| *pid);
    for (pid, e) in arrivals.iter().take(5) {
        let started_in_window = e
            .start_unix_ms
            .is_some_and(|st| st as f64 >= wall_at(t_step));
        let label = if started_in_window {
            format!("Process started {} (pid {pid})", e.name)
        } else if e.start_unix_ms.is_some() {
            format!("Process {} (pid {pid}) became active", e.name)
        } else {
            format!("Process {} (pid {pid}) entered top-15", e.name)
        };
        let cpu_txt = match e.cpu {
            Some(c) => format!("using {c:.1}% CPU in the after-window"),
            None => "no CPU delta yet (warming up)".to_string(),
        };
        let basis = if started_in_window {
            "start time falls in the after-window"
        } else if e.start_unix_ms.is_some() {
            "already running before the step; entering top-15 is not a start"
        } else {
            "process start time unavailable; presence in top-15 is not a start"
        };
        ev.push(Evidence {
            label,
            detail: format!("{cpu_txt} (parent pid {}; {basis})", e.ppid),
            delta_w: None,
            confidence: if started_in_window {
                Confidence::Medium
            } else {
                Confidence::Low
            },
        });
    }
    // Lifecycle across consecutive ticks complements the before/after
    // window diff above: identities absent for >3 ticks are estimated
    // stops (true exit needs ETW; absence from top-15 is not proof).
    let (_, stops) = lifecycle_across_ticks(&s.procs);
    for st in stops.iter().take(5) {
        ev.push(Evidence {
            label: format!("Process {} (pid {}) stopped (estimated)", st.name, st.pid),
            detail: format!(
                "last seen at t={:.0}s, absent {} ticks after; exit ~{:.0}s estimated (top-15 absence only; true exit needs ETW)",
                st.last_seen_t, st.missed_ticks, st.exit_est_t
            ),
            delta_w: None,
            confidence: Confidence::Low,
        });
    }
    // Watt remainder: no watt-measured components exist, so the full step
    // delta is unattributed by construction. Never subtract estimates.
    d.unattributed_w = Some(step.delta_w);
    d.step = Some(step);
    // Watt-carrying evidence first (by magnitude), then the rest in
    // collection order.
    ev.sort_by(|a, b| match (a.delta_w, b.delta_w) {
        (Some(x), Some(y)) => y.abs().total_cmp(&x.abs()),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    d.evidence = ev;
    d
}

// ------------------------------- baselines -------------------------------

#[derive(Debug, Clone)]
pub struct Baseline {
    pub label: String,
    pub wall_ms: u64,
    pub n: usize,
    pub median: f64,
    pub p10: f64,
    pub p90: f64,
    pub cpu_median: f64,
}

pub fn baseline_of(label: &str, s: &SessionData) -> Option<Baseline> {
    let vals: Vec<f64> = s.battery.iter().filter_map(|p| p.discharge.val()).collect();
    let sum = PowerSummary::of(&vals)?;
    let cpu_vals: Vec<f64> = s.cpu.iter().filter_map(|p| p.utility.val()).collect();
    Some(Baseline {
        label: label.to_string(),
        wall_ms: 0,
        n: sum.n,
        median: sum.median,
        p10: sum.p10,
        p90: sum.p90,
        cpu_median: stats::median(&cpu_vals).unwrap_or(f64::NAN),
    })
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o
}

pub fn baseline_to_json(b: &Baseline) -> String {
    format!(
        "{{\"label\":\"{}\",\"wall_ms\":{},\"n\":{},\"median_w\":{:.3},\"p10_w\":{:.3},\"p90_w\":{:.3},\"cpu_median_pct\":{:.2}}}",
        esc(&b.label),
        b.wall_ms,
        b.n,
        b.median,
        b.p10,
        b.p90,
        b.cpu_median
    )
}

pub fn baseline_from_json(v: &JVal) -> Option<Baseline> {
    Some(Baseline {
        label: v.get("label")?.as_str()?.to_string(),
        wall_ms: v.get("wall_ms")?.num()? as u64,
        n: v.get("n")?.num()? as usize,
        median: v.get("median_w")?.num()?,
        p10: v.get("p10_w")?.num()?,
        p90: v.get("p90_w")?.num()?,
        cpu_median: v.get("cpu_median_pct")?.num()?,
    })
}

// ------------------------------- A/B compare ------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validity {
    Valid,
    Questionable,
    Invalid,
}

impl Validity {
    pub fn as_str(self) -> &'static str {
        match self {
            Validity::Valid => "VALID",
            Validity::Questionable => "QUESTIONABLE",
            Validity::Invalid => "INVALID",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Comparison {
    pub label_a: String,
    pub label_b: String,
    pub med_a: Option<f64>,
    pub med_b: Option<f64>,
    pub delta_w: Option<f64>,
    pub rel: Option<f64>,
    pub runtime_a_h: Option<f64>,
    pub runtime_b_h: Option<f64>,
    /// Full-charge normalized runtimes (same energy basis for both sides).
    pub runtime_norm_a_h: Option<f64>,
    pub runtime_norm_b_h: Option<f64>,
    /// Normalized B-minus-A projection in minutes. None without both norms.
    pub projected_delta_min: Option<f64>,
    pub lines: Vec<String>,
    pub confidence: Confidence,
    pub confidence_note: String,
    /// Experiment validity is reported independently of whether an effect was
    /// found: contaminated or thin captures are rejected before interpretation.
    pub validity: Validity,
}

/// Per-side capture quality for A/B confidence. Records enough about how a
/// phase was actually captured that repeated cached samples cannot inflate
/// confidence: elapsed duration, distinct values, spread, worst gap and how
/// much of the observed span was actively discharging.
#[derive(Debug, Clone, Copy)]
pub struct CaptureQuality {
    pub n: usize,
    pub duration_secs: f64,
    pub unique_values: usize,
    pub spread: f64,
    pub abs_spread_w: f64,
    pub max_gap_secs: f64,
    pub coverage: Option<f64>,
    /// Lag-1 autocorrelation of the discharge series (sensor persistence).
    pub rho_lag1: f64,
    /// Observed sample cadence ((n-1)/duration), when measurable.
    pub observed_hz: Option<f64>,
    /// Median inter-sample gap, reference for the gap-vs-cadence check.
    pub median_dt_secs: f64,
    /// Forced segment boundaries from the both-clock scan (sampling gaps and
    /// wall/mono divergence). Never integrated across.
    pub discontinuities: usize,
    /// Real elapsed time lost at those boundaries.
    pub unobserved_secs: f64,
}

/// Wall-vs-monotonic divergence beyond this is treated as suspend/resume or a
/// clock step. Platform clocks differ in whether they advance during sleep,
/// so we do not assume; 2 s is far above scheduling jitter but far below any
/// real suspend interval.
pub const MAX_CLOCK_SKEW_SECS: f64 = 2.0;

/// Distinct count at 10 mW resolution, so a stuck/cached sensor counts once.
fn unique_count(vals: &[f64]) -> usize {
    let mut seen: Vec<i64> = vals.iter().map(|v| (v * 100.0).round() as i64).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

fn capture_quality(s: &SessionData, vals: &[f64]) -> CaptureQuality {
    let times: Vec<f64> = s.battery.iter().map(|p| p.t).collect();
    let duration = match (times.first(), times.last()) {
        (Some(a), Some(b)) if b > a => b - a,
        _ => 0.0,
    };
    let summary = PowerSummary::of(vals);
    let (spread, abs_spread) = summary
        .as_ref()
        .map(|x| {
            let rel = if x.median.abs() > f64::EPSILON {
                (x.p90 - x.p10) / x.median
            } else {
                f64::INFINITY
            };
            (rel, x.p90 - x.p10)
        })
        .unwrap_or((f64::INFINITY, f64::INFINITY));
    let gaps: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    let max_gap = gaps.iter().copied().fold(0.0f64, f64::max);
    let median_dt = stats::median(&gaps).unwrap_or(0.0);
    // Coverage over the full battery timeline: both endpoints discharging and
    // within a plausible cadence gap. The scan uses BOTH clocks: a gap in
    // either, or a wall/mono divergence (suspend/resume or clock step),
    // forces a segment boundary and is never bridged.
    let opts: Vec<Option<f64>> = s.battery.iter().map(|p| p.discharge.val()).collect();
    let wall: Vec<f64> = s
        .battery
        .iter()
        .map(|p| p.discharge.stamp.wall_millis as f64 / 1000.0)
        .collect();
    let gap = (median_dt * 3.0).max(5.0);
    let seg = stats::integrate_segmented_clocks(&times, &wall, &opts, gap, MAX_CLOCK_SKEW_SECS);
    CaptureQuality {
        n: vals.len(),
        duration_secs: duration,
        unique_values: unique_count(vals),
        spread,
        abs_spread_w: abs_spread,
        max_gap_secs: max_gap,
        coverage: seg.coverage(),
        rho_lag1: stats::lag1_autocorr(vals),
        observed_hz: (duration > 0.0 && vals.len() > 1)
            .then(|| (vals.len() as f64 - 1.0) / duration),
        median_dt_secs: median_dt,
        discontinuities: seg.discontinuities,
        unobserved_secs: seg.unobserved_secs,
    }
}

/// How fresh a series actually is. Repeated identical readings are not
/// independent observations: `value_age_s` can greatly exceed
/// `sample_age_s` when a sensor reports a cached value.
#[derive(Debug, Clone, Copy)]
pub struct Freshness {
    pub sample_age_s: f64,
    pub value_age_s: f64,
    pub updates: usize,
    pub observed_hz: Option<f64>,
}

/// Freshness of the discharge series (the analytically dominant signal).
pub fn discharge_freshness(s: &SessionData) -> Option<Freshness> {
    let pts: Vec<(f64, f64)> = s
        .battery
        .iter()
        .filter_map(|p| p.discharge.val().map(|w| (p.t, w)))
        .collect();
    let last = pts.last().copied()?;
    let session_end = s.battery.iter().map(|p| p.t).fold(last.0, f64::max);
    let sample_age_s = (session_end - last.0).max(0.0);
    // Walk back to the most recent reading that differs at 10 mW resolution;
    // the current value has been held since the sample after it.
    let mut changed_at = pts[0].0;
    for i in (0..pts.len().saturating_sub(1)).rev() {
        if ((pts[i].1 - last.1) * 100.0).round() != 0.0 {
            changed_at = pts[i + 1].0;
            break;
        }
    }
    let value_age_s = (last.0 - changed_at).max(0.0);
    let duration = (last.0 - pts[0].0).max(0.0);
    let vals: Vec<f64> = pts.iter().map(|(_, w)| *w).collect();
    let observed_hz = (duration > 0.0).then(|| (pts.len() as f64 - 1.0) / duration);
    let updates = unique_count(&vals);
    Some(Freshness {
        sample_age_s,
        value_age_s,
        updates,
        observed_hz,
    })
}

/// Drop the first `settle_secs` of every series so transients from a manual
/// condition change cannot contaminate the measurement window.
pub fn trim_settle(s: &SessionData, settle_secs: f64) -> SessionData {
    if settle_secs <= 0.0 {
        return s.clone();
    }
    let mut t = s.clone();
    t.battery.retain(|p| p.t >= settle_secs);
    t.cpu.retain(|p| p.t >= settle_secs);
    t.gpu.retain(|p| p.t >= settle_secs);
    t.procs.retain(|p| p.t >= settle_secs);
    t.display.retain(|p| p.t >= settle_secs);
    t.net.retain(|p| p.t >= settle_secs);
    t.storage.retain(|p| p.t >= settle_secs);
    t.usb.retain(|p| p.t >= settle_secs);
    t.selfmon.retain(|p| p.t >= settle_secs);
    t
}

/// Combine several repeated blocks of one condition into one quality record.
/// Duration and distinct samples add up (more time on condition is more
/// evidence); spread and worst gap take the weakest block.
pub fn aggregate_quality(blocks: &[CaptureQuality]) -> CaptureQuality {
    let mut out = CaptureQuality {
        n: 0,
        duration_secs: 0.0,
        unique_values: 0,
        spread: 0.0,
        abs_spread_w: 0.0,
        max_gap_secs: 0.0,
        coverage: None,
        rho_lag1: 0.0,
        observed_hz: None,
        median_dt_secs: 0.0,
        discontinuities: 0,
        unobserved_secs: 0.0,
    };
    let mut cov = f64::INFINITY;
    let mut any = false;
    for b in blocks {
        out.n += b.n;
        out.duration_secs += b.duration_secs;
        out.unique_values += b.unique_values;
        out.spread = out.spread.max(b.spread);
        out.abs_spread_w = out.abs_spread_w.max(b.abs_spread_w);
        out.max_gap_secs = out.max_gap_secs.max(b.max_gap_secs);
        out.rho_lag1 = out.rho_lag1.max(b.rho_lag1);
        out.discontinuities += b.discontinuities;
        out.unobserved_secs += b.unobserved_secs;
        out.observed_hz = match (out.observed_hz, b.observed_hz) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, None) => x,
            (None, y) => y,
        };
        if b.median_dt_secs > 0.0
            && (out.median_dt_secs <= 0.0 || b.median_dt_secs < out.median_dt_secs)
        {
            out.median_dt_secs = b.median_dt_secs;
        }
        if let Some(c) = b.coverage {
            cov = cov.min(c);
            any = true;
        }
    }
    out.coverage = any.then_some(cov);
    out
}

/// Median discharge of a block, and its quality, after settling is removed.
pub fn block_median(s: &SessionData) -> (Option<f64>, CaptureQuality) {
    let vals: Vec<f64> = s.battery.iter().filter_map(|p| p.discharge.val()).collect();
    (stats::median(&vals), capture_quality(s, &vals))
}

/// Sensor-update shortfall: too few distinct discharge readings for the
/// observed cadence means the sensor was cached, not measuring. Requires at
/// least max(5, 10% of expected samples) distinct updates; None when the
/// cadence is unmeasurable (no gate without evidence).
fn freshness_shortfall(q: &CaptureQuality, side: &str) -> Option<String> {
    let hz = q.observed_hz?;
    if hz.is_nan() || hz <= 0.0 || q.duration_secs.is_nan() || q.duration_secs <= 0.0 {
        return None;
    }
    let required = 5.0f64.max((q.duration_secs * hz * 0.1).ceil());
    if (q.unique_values as f64) < required {
        Some(format!(
            "stale sensor on {side}: {} distinct updates in {:.0}s at ~{hz:.2} Hz (need >={required:.0})",
            q.unique_values, q.duration_secs,
        ))
    } else {
        None
    }
}

/// Conservative, deterministic A/B confidence. A short capture cannot reach
/// High merely by having many rows; the effective sample count discounts
/// lag-1 autocorrelation (persistent readings are not independent) and is
/// capped by distinct values, and coverage/duration/spread/sensor-freshness
/// gate the level. Confounders downgrade the result and are reported rather
/// than hidden.
pub fn ab_confidence(
    a: &CaptureQuality,
    b: &CaptureQuality,
    effect_w: Option<f64>,
    cpu_mismatch_pp: Option<f64>,
) -> (Confidence, String, Vec<String>) {
    let mut confounders: Vec<String> = Vec::new();
    let eff_n = stats::effective_n(a.n, a.unique_values, a.rho_lag1).min(stats::effective_n(
        b.n,
        b.unique_values,
        b.rho_lag1,
    ));
    let duration = a.duration_secs.min(b.duration_secs);
    let spread = a.spread.max(b.spread);
    let coverage = match (a.coverage, b.coverage) {
        (Some(x), Some(y)) => Some(x.min(y)),
        _ => None,
    };
    if duration < 5.0 {
        confounders.push(format!("very short capture ({duration:.0}s)"));
    }
    if eff_n < 5 {
        confounders.push(format!(
            "few distinct values (effective n={eff_n} of {}/{})",
            a.n, b.n
        ));
    }
    for (q, side) in [(a, "A"), (b, "B")] {
        if q.median_dt_secs > 0.0 && q.max_gap_secs > 3.0 * q.median_dt_secs {
            confounders.push(format!(
                "sampling gap on {side}: {:.0}s exceeds 3x median cadence {:.1}s",
                q.max_gap_secs, q.median_dt_secs
            ));
        }
        if let Some(msg) = freshness_shortfall(q, side) {
            confounders.push(msg);
        }
        if q.discontinuities > 0 {
            confounders.push(format!(
                "clock discontinuity on {side}: {} boundary(ies), {} unobserved (sleep/gap/clock step)",
                q.discontinuities,
                fmt_duration(q.unobserved_secs)
            ));
        }
    }
    if spread >= 0.6 {
        confounders.push(format!("unstable spread {spread:.2}"));
    }
    if let Some(c) = coverage
        && c < 0.7
    {
        confounders.push(format!("low discharge coverage {:.0}%", c * 100.0));
    }
    if let Some(m) = cpu_mismatch_pp
        && m >= 5.0
    {
        confounders.push(format!("background CPU mismatch {m:.1} pp"));
    }
    let noise = a.abs_spread_w.max(b.abs_spread_w);
    if let Some(e) = effect_w
        && e.abs() < noise
    {
        confounders.push(format!(
            "effect {e:+.2} W is within measured spread {noise:.2} W"
        ));
    }
    let base = if duration >= 60.0
        && eff_n >= 10
        && spread < 0.3
        && coverage.is_none_or(|c| c >= 0.9)
    {
        Confidence::High
    } else if duration >= 20.0 && eff_n >= 5 && spread < 0.6 && coverage.is_none_or(|c| c >= 0.7) {
        Confidence::Medium
    } else {
        Confidence::Low
    };
    let level = if confounders.is_empty() {
        base
    } else {
        match base {
            Confidence::High => Confidence::Medium,
            Confidence::Medium => Confidence::Low,
            Confidence::Low => Confidence::Low,
        }
    };
    let note = format!(
        "n={}/{}, distinct={}/{}, {:.0}s, spread {spread:.2}, coverage {}",
        a.n,
        b.n,
        a.unique_values,
        b.unique_values,
        duration,
        coverage
            .map(|c| format!("{:.0}%", c * 100.0))
            .unwrap_or_else(|| "n/a".to_string())
    );
    (level, note, confounders)
}

fn series_median(points: &[(f64, Option<f64>)]) -> Option<f64> {
    stats::median(&points.iter().filter_map(|(_, v)| *v).collect::<Vec<_>>())
}

pub fn compare_sessions(a: &SessionData, b: &SessionData) -> Comparison {
    // Compat wrapper: settle == 0 selects automatic settling detection,
    // which trims nothing on already-stable series.
    compare_sessions_with_settle(a, b, 0.0)
}

/// Single-rep A/B with settling removed. NOTE to main.rs: the reps==1
/// experiment path records two back-to-back phases without trimming; it
/// should call this (passing --settle, 0 = auto-detect) instead of
/// `compare_sessions`, so the manual condition-change transient cannot
/// contaminate the measurement window.
pub fn compare_single_with_settle(a: &SessionData, b: &SessionData, settle_s: f64) -> Comparison {
    compare_sessions_with_settle(a, b, settle_s)
}

/// Full-charge capacity in Wh: `battery_meta.full_mwh` first; otherwise the
/// observed remaining-energy span scaled by the SOC span (needs >= 5 pp of
/// charge movement to anchor the scale). None when neither basis exists —
/// never invent capacity.
fn full_capacity_wh(s: &SessionData) -> Option<f64> {
    if let Some(mwh) = s.battery_meta.full_mwh
        && mwh > 0.0
        && mwh.is_finite()
    {
        return Some(mwh / 1000.0);
    }
    let rems: Vec<f64> = s
        .battery
        .iter()
        .filter_map(|p| p.remaining_wh.val())
        .collect();
    let pcts: Vec<f64> = s.battery.iter().filter_map(|p| p.pct.val()).collect();
    if rems.len() >= 2 && pcts.len() >= 2 {
        let r_span = rems.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - rems.iter().copied().fold(f64::INFINITY, f64::min);
        let p_span = pcts.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - pcts.iter().copied().fold(f64::INFINITY, f64::min);
        if p_span >= 5.0 && r_span > 0.0 {
            return Some(r_span / (p_span / 100.0));
        }
    }
    None
}

/// SOC drift across phases: > 5 pp median charge difference confounds a
/// watt comparison (the discharge curve is voltage-dependent).
fn soc_confounder(a: &SessionData, b: &SessionData) -> Option<String> {
    let med = |s: &SessionData| {
        stats::median(
            &s.battery
                .iter()
                .filter_map(|p| p.pct.val())
                .collect::<Vec<_>>(),
        )
    };
    match (med(a), med(b)) {
        (Some(x), Some(y)) if (y - x).abs() > 5.0 => Some(format!(
            "SOC drift {x:.0}% -> {y:.0}% across phases (>5 pp; the voltage curve confounds a watt comparison)"
        )),
        _ => None,
    }
}

/// First index where the series has settled. Index 0 already stable (the
/// first two short windows agree) returns 0; an initial transient that
/// converges to the trailing stable level returns its start; a series that
/// never settles (or is too short) returns 0 so no data is discarded on a
/// guess. Windows span ~5 s of samples; tolerance is 0.5 W or 10% of the
/// stable level, whichever is larger. Deterministic.
pub fn detect_settle_end(series: &[f64], dt_secs: f64) -> usize {
    let n = series.len();
    let w = ((5.0 / dt_secs.max(0.05)) as usize).clamp(3, 30);
    if n < 2 * w + 1 {
        return 0;
    }
    let med = |s: &[f64]| stats::median(s).unwrap_or(f64::NAN);
    let stable = med(&series[n - w..]);
    let tol = 0.5f64.max(0.1 * stable.abs());
    // Opening already at the stable level: nothing to trim.
    if (med(&series[..w]) - stable).abs() <= tol {
        return 0;
    }
    // Opening deviates: only a SHORT prefix may be a transient. A late
    // convergence is a regime change (e.g. an A/B step), not settling, and
    // must not be silently discarded — cap the search at 3 windows.
    let cap = (n / 2).min(3 * w);
    for i in 1..=cap.saturating_sub(w) {
        if (med(&series[i..i + w]) - stable).abs() <= tol {
            return i;
        }
    }
    0
}

/// Automatic settling trim in seconds for a discharge series.
pub fn auto_settle_secs(vals: &[f64], dt_secs: f64) -> f64 {
    detect_settle_end(vals, dt_secs) as f64 * dt_secs.max(0.05)
}

/// Median CPU utility of one block, for background-mismatch tracking in
/// the multi-rep experiment path.
pub fn block_cpu_median(s: &SessionData) -> Option<f64> {
    series_median(
        &s.cpu
            .iter()
            .map(|p| (p.t, p.utility.val()))
            .collect::<Vec<_>>(),
    )
}

/// Multi-rep experiment confidence with background CPU mismatch wired in
/// (instead of None). NOTE to main.rs: the balanced-reps path should
/// collect per-block `block_cpu_median` values for both conditions and
/// call this with those slices, so reps>1 checks background mismatch.
pub fn experiment_confidence(
    qa: &CaptureQuality,
    qb: &CaptureQuality,
    delta: Option<f64>,
    a_cpu_meds: &[f64],
    b_cpu_meds: &[f64],
) -> (Confidence, String, Vec<String>) {
    ab_confidence(
        qa,
        qb,
        delta,
        aggregate_cpu_mismatch(a_cpu_meds, b_cpu_meds),
    )
}

/// Experiment validity invariants, shared by the single-rep comparison and the
/// repeated-block experiment so the coverage/duration/thin-capture gates cannot
/// drift apart. Thin or contaminated captures are Invalid; confounders or Low
/// confidence make the result Questionable; otherwise Valid.
pub fn comparison_validity(
    med_a: Option<f64>,
    med_b: Option<f64>,
    qa: &CaptureQuality,
    qb: &CaptureQuality,
    confounders: &[String],
    confidence: Confidence,
) -> Validity {
    if med_a.is_none()
        || med_b.is_none()
        || qa.coverage.is_some_and(|c| c < 0.5)
        || qb.coverage.is_some_and(|c| c < 0.5)
        || qa.duration_secs.min(qb.duration_secs) < 5.0
    {
        Validity::Invalid
    } else if !confounders.is_empty() || confidence == Confidence::Low {
        Validity::Questionable
    } else {
        Validity::Valid
    }
}

/// Normalized full-charge runtimes and the B-minus-A projection in minutes,
/// from per-side discharge medians and each session's capacity basis. Shared by
/// both A/B paths so runtime reporting is identical and never invented when the
/// capacity or median is missing.
pub fn runtime_projection(
    a: &SessionData,
    b: &SessionData,
    med_a: Option<f64>,
    med_b: Option<f64>,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let norm = |s: &SessionData, med: Option<f64>| match (full_capacity_wh(s), med) {
        (Some(f), Some(m)) => stats::normalized_runtime_h(m, f),
        _ => None,
    };
    let norm_a = norm(a, med_a);
    let norm_b = norm(b, med_b);
    let projected = match (norm_a, norm_b) {
        (Some(x), Some(y)) => Some((y - x) * 60.0),
        _ => None,
    };
    (norm_a, norm_b, projected)
}

/// Assemble a [`Comparison`] from per-side medians, capture quality, and a
/// confidence triple. Single-rep settling and repeated-block experiments both
/// finish here so runtime projection and validity invariants are shared.
#[allow(clippy::too_many_arguments)]
pub fn comparison_from_stats(
    a: &SessionData,
    b: &SessionData,
    med_a: Option<f64>,
    med_b: Option<f64>,
    qa: &CaptureQuality,
    qb: &CaptureQuality,
    confidence: Confidence,
    confidence_note: String,
    confounders: Vec<String>,
    mut lines: Vec<String>,
) -> Comparison {
    let (delta_w, rel) = match (med_a, med_b) {
        (Some(x), Some(y)) if x > 0.0 => (Some(y - x), Some((y - x) / x)),
        _ => (None, None),
    };
    // Runtime projection from each session's own last remaining energy.
    let rem = |s: &SessionData| {
        s.battery
            .iter()
            .filter_map(|p| p.remaining_wh.val())
            .next_back()
    };
    let runtime_a_h = match (rem(a), med_a) {
        (Some(r), Some(m)) if m > 0.0 => stats::runtime_hours(r, m),
        _ => None,
    };
    let runtime_b_h = match (rem(b), med_b) {
        (Some(r), Some(m)) if m > 0.0 => stats::runtime_hours(r, m),
        _ => None,
    };
    // Normalized full-charge runtime: same energy basis for both sides, so
    // different starting charges do not masquerade as efficiency deltas.
    let (runtime_norm_a_h, runtime_norm_b_h, projected_delta_min) =
        runtime_projection(a, b, med_a, med_b);
    for c in &confounders {
        lines.push(format!("[confounder] {c}"));
    }
    let validity = comparison_validity(med_a, med_b, qa, qb, &confounders, confidence);
    Comparison {
        label_a: a.label.clone(),
        label_b: b.label.clone(),
        med_a,
        med_b,
        delta_w,
        rel,
        runtime_a_h,
        runtime_b_h,
        runtime_norm_a_h,
        runtime_norm_b_h,
        projected_delta_min,
        lines,
        confidence,
        confidence_note,
        validity,
    }
}

pub fn compare_sessions_with_settle(a: &SessionData, b: &SessionData, settle_s: f64) -> Comparison {
    // Defensive settling trim: explicit seconds when positive, otherwise
    // automatic detection per side (stable series trim nothing).
    let trim_secs = |s: &SessionData| {
        if settle_s > 0.0 {
            settle_s
        } else {
            let vals: Vec<f64> = s.battery.iter().filter_map(|p| p.discharge.val()).collect();
            let ts: Vec<f64> = s.battery.iter().map(|p| p.t).collect();
            auto_settle_secs(&vals, median_interval(&ts))
        }
    };
    let ta = trim_settle(a, trim_secs(a));
    let tb = trim_settle(b, trim_secs(b));
    let (a, b) = (&ta, &tb);
    let va: Vec<f64> = a.battery.iter().filter_map(|p| p.discharge.val()).collect();
    let vb: Vec<f64> = b.battery.iter().filter_map(|p| p.discharge.val()).collect();
    let sa = PowerSummary::of(&va);
    let sb = PowerSummary::of(&vb);
    let (med_a, med_b) = (sa.as_ref().map(|s| s.median), sb.as_ref().map(|s| s.median));
    let delta_w = match (med_a, med_b) {
        (Some(x), Some(y)) if x > 0.0 => Some(y - x),
        _ => None,
    };
    let qa = capture_quality(a, &va);
    let qb = capture_quality(b, &vb);
    let cpu_mismatch_pp = match (
        series_median(
            &a.cpu
                .iter()
                .map(|p| (p.t, p.utility.val()))
                .collect::<Vec<_>>(),
        ),
        series_median(
            &b.cpu
                .iter()
                .map(|p| (p.t, p.utility.val()))
                .collect::<Vec<_>>(),
        ),
    ) {
        (Some(x), Some(y)) => Some((y - x).abs()),
        _ => None,
    };
    let (confidence, confidence_note, mut confounders) =
        ab_confidence(&qa, &qb, delta_w, cpu_mismatch_pp);
    if let Some(msg) = soc_confounder(a, b) {
        confounders.push(msg);
    }

    let mut lines = Vec::new();
    let cpu_med = |s: &SessionData| {
        series_median(
            &s.cpu
                .iter()
                .map(|p| (p.t, p.utility.val()))
                .collect::<Vec<_>>(),
        )
    };
    if let (Some(x), Some(y)) = (cpu_med(a), cpu_med(b)) {
        lines.push(format!("[derived] CPU utility median: {x:.1}% -> {y:.1}%"));
    }
    let c3_med = |s: &SessionData| {
        series_median(
            &s.cpu
                .iter()
                .map(|p| (p.t, p.c3_pct.val()))
                .collect::<Vec<_>>(),
        )
    };
    if let (Some(x), Some(y)) = (c3_med(a), c3_med(b)) {
        lines.push(format!(
            "[derived] CPU % C3 residency median: {x:.0}% -> {y:.0}%"
        ));
    }
    // GPU per adapter (matched by name).
    let mut gnames: Vec<String> = Vec::new();
    for s in [a, b] {
        for g in &s.gpu {
            for ad in &g.adapters {
                if !gnames.contains(&ad.name) {
                    gnames.push(ad.name.clone());
                }
            }
        }
    }
    for name in &gnames {
        let med = |s: &SessionData| {
            series_median(
                &s.gpu
                    .iter()
                    .filter_map(|g| {
                        g.adapters
                            .iter()
                            .find(|x| &x.name == name)
                            .and_then(|x| x.total_util.val().map(|u| (g.t, Some(u))))
                    })
                    .collect::<Vec<_>>(),
            )
        };
        if let (Some(x), Some(y)) = (med(a), med(b)) {
            lines.push(format!(
                "[derived] GPU {name} engine-sum median: {x:.1}% -> {y:.1}%"
            ));
        }
    }
    let bri_med = |s: &SessionData| {
        series_median(
            &s.display
                .iter()
                .map(|p| (p.t, p.brightness.val()))
                .collect::<Vec<_>>(),
        )
    };
    if let (Some(x), Some(y)) = (bri_med(a), bri_med(b)) {
        lines.push(format!(
            "[derived] display brightness median: {x:.0}% -> {y:.0}%"
        ));
    }
    // Policy diffs.
    let policy_diff = |label: &str, f: fn(&crate::session::Policy) -> Option<f64>| match (
        f(&a.policy),
        f(&b.policy),
    ) {
        (Some(x), Some(y)) if (x - y).abs() > f64::EPSILON => {
            Some(format!("[measured] {label}: {x:.0} -> {y:.0}"))
        }
        _ => None,
    };
    for line in [
        policy_diff("cpu_min_dc", |p| p.cpu_min_dc.val()),
        policy_diff("cpu_max_dc", |p| p.cpu_max_dc.val()),
        policy_diff("brightness_dc", |p| p.brightness_dc.val()),
        policy_diff("display_timeout_dc", |p| p.display_timeout_dc.val()),
        policy_diff("sleep_timeout_dc", |p| p.sleep_timeout_dc.val()),
    ]
    .into_iter()
    .flatten()
    {
        lines.push(line);
    }
    if a.policy.scheme_name != b.policy.scheme_name {
        lines.push(format!(
            "[measured] power scheme: {} -> {}",
            a.policy.scheme_name.as_deref().unwrap_or("?"),
            b.policy.scheme_name.as_deref().unwrap_or("?")
        ));
    }
    // Process presence: names seen in top lists.
    let names = |s: &SessionData| {
        let mut set = std::collections::BTreeSet::new();
        for p in &s.procs {
            for e in &p.top {
                set.insert(e.name.clone());
            }
        }
        set
    };
    let (na_set, nb_set) = (names(a), names(b));
    let new: Vec<&String> = nb_set.difference(&na_set).take(6).collect();
    let gone: Vec<&String> = na_set.difference(&nb_set).take(6).collect();
    if !new.is_empty() {
        lines.push(format!(
            "[measured] processes appearing in B: {}",
            new.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !gone.is_empty() {
        lines.push(format!(
            "[measured] processes gone in B: {}",
            gone.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // Thermal state has no sensor: CPU utility drift is the coarse proxy
    // (see the CPU mismatch confounder); the gap is stated, not hidden.
    lines.push("[note] thermal state unobservable (no CPU temperature sensor; background CPU drift above is the only thermal proxy)".to_string());
    // Session energy footers when both recorded discharge energy.
    let wh = |s: &SessionData| s.footer.as_ref().and_then(|f| f.get("discharge_wh")?.num());
    if let (Some(x), Some(y)) = (wh(a), wh(b)) {
        lines.push(format!(
            "[derived] session discharge energy: {x:.3} Wh -> {y:.3} Wh"
        ));
    }
    comparison_from_stats(
        a,
        b,
        med_a,
        med_b,
        &qa,
        &qb,
        confidence,
        confidence_note,
        confounders,
        lines,
    )
}

// --------------------------------- rendering --------------------------------

fn fmt_h(hours: f64) -> String {
    format!("{:.0}h{:02.0}m", hours.floor(), (hours % 1.0) * 60.0)
}

/// Compact m/s duration for unobserved-interval reporting.
fn fmt_duration(secs: f64) -> String {
    if secs >= 3600.0 {
        fmt_h(secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m{:.0}s", (secs / 60.0).floor(), secs % 60.0)
    } else {
        format!("{secs:.0}s")
    }
}

pub fn render_diagnosis(label: &str, d: &Diagnosis) -> String {
    let mut o = format!("DIAGNOSIS: {label}\n");
    match d.median_w {
        Some(m) => o.push_str(&format!(
            "[derived] discharge median: {m:.2} W (n={})\n",
            d.discharge_n
        )),
        None => o.push_str(&format!("[unavailable] {}\n", d.note)),
    }
    if let (Some(a), Some(r)) = (d.baseline_abs_w, d.baseline_rel) {
        o.push_str(&format!(
            "[derived] vs baseline: {a:+.2} W ({r:+.1}%)\n",
            r = r * 100.0
        ));
    }
    if let Some(st) = &d.step {
        o.push_str(&format!(
            "[derived] step at sample {}: {before:.2} W -> {after:.2} W ({delta:+.2} W, confidence {conf})\n",
            st.index,
            before = st.before_median_w,
            after = st.after_median_w,
            delta = st.delta_w,
            conf = st.confidence.as_str()
        ));
    } else if !d.note.is_empty() && d.median_w.is_some() {
        o.push_str(&format!("[derived] {}\n", d.note));
    }
    if d.evidence.is_empty() {
        o.push_str("no correlated evidence within the step window\n");
    } else {
        o.push_str("correlated changes (correlation, not proven causation):\n");
        for e in &d.evidence {
            let w = match e.delta_w {
                Some(v) => format!(" [{v:+.2} W]"),
                None => String::new(),
            };
            o.push_str(&format!(
                "  - {} (confidence {}): {}{}\n",
                e.label,
                e.confidence.as_str(),
                e.detail,
                w
            ));
        }
    }
    if let Some(u) = d.unattributed_w {
        o.push_str(&format!(
            "[derived] unattributed system power change: {u:+.2} W (no watt-measured components; estimates listed separately, never subtracted)\n"
        ));
    }
    o
}

pub fn render_comparison(c: &Comparison) -> String {
    let mut o = format!("A/B COMPARISON: {} vs {}\n", c.label_a, c.label_b);
    match (c.med_a, c.med_b, c.delta_w, c.rel) {
        (Some(a), Some(b), Some(d), Some(r)) => o.push_str(&format!(
            "[derived] discharge median: {a:.2} W -> {b:.2} W ({d:+.2} W, {r:+.1}%)\n",
            r = r * 100.0
        )),
        _ => o.push_str("[unavailable] discharge medians missing on one side\n"),
    }
    match (c.runtime_a_h, c.runtime_b_h) {
        (Some(a), Some(b)) => o.push_str(&format!(
            "[derived] projected runtime: {} -> {} (from each session's own remaining energy)\n",
            fmt_h(a),
            fmt_h(b)
        )),
        _ => o.push_str("[unavailable] runtime projection needs remaining energy + discharge\n"),
    }
    match (c.runtime_norm_a_h, c.runtime_norm_b_h, c.projected_delta_min) {
        (Some(a), Some(b), Some(d)) => o.push_str(&format!(
            "[derived] normalized full-charge runtime: {} -> {} ({d:+.1} min; same energy basis)\n",
            fmt_h(a),
            fmt_h(b)
        )),
        _ => o.push_str("[unavailable] normalized runtime needs full-charge capacity (or SOC-scaled span) + discharge\n"),
    }
    for line in &c.lines {
        o.push_str(line);
        o.push('\n');
    }
    o.push_str(&format!(
        "validity: {} (independent of whether an effect was found)\n",
        c.validity.as_str()
    ));
    o.push_str(&format!(
        "confidence: {} ({})\n",
        c.confidence.as_str(),
        c.confidence_note
    ));
    o
}

/// Redact identifying strings from a rendered report before sharing it.
///
/// Process names become stable `process-N` placeholders; PIDs (any case,
/// `pid 123` / `PID=123` / `"pid":123`), GUIDs, Windows profile paths and
/// usernames, hostnames, SSIDs, MAC and IPv4 addresses, and USB/GPU/display
/// device names are masked. Field labels and structure are preserved; values
/// are replaced with consistent `<class>` placeholders. Deterministic and
/// idempotent: the same input redacts the same way, and re-redacting text
/// that was already redacted is a no-op.
pub fn redact_report(text: &str, process_names: &[String]) -> String {
    redact_finish(mask_process_names(text, process_names))
}

/// One sensitive token and the placeholder that replaces it. `value` is
/// matched as a whole identifier (bounded), so short or common tokens cannot
/// corrupt unrelated words; class-based placeholders keep output readable.
#[derive(Debug, Clone)]
pub struct RedactToken {
    pub value: String,
    pub placeholder: String,
}

impl RedactToken {
    pub fn new(value: impl Into<String>, placeholder: impl Into<String>) -> Self {
        RedactToken {
            value: value.into(),
            placeholder: placeholder.into(),
        }
    }
}

/// Redact a rendered report given the full set of sensitive tokens drawn from
/// the session model: process names (`process-N`), and GPU/display/network/
/// Wi-Fi/USB/battery/host/user/label identity as class placeholders. Bare
/// rendered values — e.g. `GPU AMD Radeon (TM) Graphics` with no `key:` — are
/// masked because the token itself is matched, not just keyed syntax.
pub fn redact_report_identifiers(text: &str, tokens: &[RedactToken]) -> String {
    redact_finish(mask_identifiers(text, tokens))
}

fn redact_finish(out: String) -> String {
    let mut out = mask_windows_paths(&out);
    out = mask_keyed_identifiers(&out);
    out = mask_titled_labels(&out);
    out = mask_guids(&out);
    out = mask_pids(&out);
    out = mask_mac(&out);
    out = mask_ipv4(&out);
    out
}

/// `DASHBOARD <label>` and `TIMELINE <label>  t=...` headers carry the raw
/// session label (often a hostname). Mask the label but keep the TIMELINE
/// metrics tail after the two-space separator.
fn mask_titled_labels(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let has_nl = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let mut replaced = None;
        for prefix in ["DASHBOARD ", "TIMELINE "] {
            if let Some(rest) = body.strip_prefix(prefix) {
                let cut = rest.find("  ").unwrap_or(rest.len());
                replaced = Some(format!("{prefix}<redacted>{}", &rest[cut..]));
                break;
            }
        }
        out.push_str(&replaced.unwrap_or_else(|| body.to_string()));
        if has_nl {
            out.push('\n');
        }
    }
    out
}

/// Case-insensitive replace of `needle` only at identifier boundaries, so a
/// short process name cannot corrupt unrelated words. Names shorter than two
/// characters are ignored (too collision-prone even at boundaries).
fn mask_process_names(text: &str, process_names: &[String]) -> String {
    let tokens: Vec<RedactToken> = process_names
        .iter()
        .enumerate()
        .map(|(i, n)| RedactToken::new(n.clone(), format!("process-{}", i + 1)))
        .collect();
    mask_identifiers(text, &tokens)
}

/// Replace every token with its placeholder at identifier boundaries,
/// longest value first (so a composite `adapter:pid` wins over its parts).
/// Empty/one-character values are skipped.
fn mask_identifiers(text: &str, tokens: &[RedactToken]) -> String {
    let mut items: Vec<(&str, &str)> = tokens
        .iter()
        .filter(|t| t.value.trim().chars().count() >= 2)
        .map(|t| (t.value.trim(), t.placeholder.as_str()))
        .collect();
    items.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(b.0)));
    let mut out = text.to_string();
    let mut last: Option<&str> = None;
    for (value, placeholder) in items {
        if last == Some(value) {
            continue;
        }
        last = Some(value);
        out = replace_ci_bounded(&out, value, placeholder);
    }
    out
}

fn replace_ci_bounded(text: &str, needle: &str, replacement: &str) -> String {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() {
        return text.to_string();
    }
    let t: Vec<char> = text.chars().collect();
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-';
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < t.len() {
        let end = i + n.len();
        let matched = end <= t.len()
            && t[i..end]
                .iter()
                .zip(&n)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
            && (i == 0 || !is_ident(t[i - 1]))
            && (end == t.len() || !is_ident(t[end]));
        if matched {
            out.push_str(replacement);
            i = end;
        } else {
            out.push(t[i]);
            i += 1;
        }
    }
    out
}

/// Case-insensitive prefix match; returns the index just past `pat`.
fn starts_with_ci(t: &[char], at: usize, pat: &str) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    if at + p.len() > t.len() {
        return None;
    }
    if t[at..at + p.len()]
        .iter()
        .zip(&p)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        Some(at + p.len())
    } else {
        None
    }
}

/// Characters that stay inside a path token (delimiters stop it).
fn is_path_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '"' | '\'' | ',' | ';' | ')' | ']' | '}' | '<' | '>' | '|'
        )
}

/// Mask Windows paths. A drive path (`C:\...`) is replaced wholesale with
/// `<path>`; a bare `\Users\<name>` (or `\Documents and Settings\<name>`)
/// keeps the profile root but masks the account component as `<user>`.
fn mask_windows_paths(text: &str) -> String {
    let t: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < t.len() {
        let boundary_before = i == 0 || !t[i - 1].is_ascii_alphanumeric() && t[i - 1] != ':';
        let drive = boundary_before
            && i + 2 < t.len()
            && t[i].is_ascii_alphabetic()
            && t[i + 1] == ':'
            && matches!(t[i + 2], '\\' | '/');
        if drive {
            i += 3;
            while i < t.len() && is_path_char(t[i]) {
                i += 1;
            }
            out.push_str("<path>");
            continue;
        }
        if t[i] == '\\' {
            // UNC share: \\HOST\share -> \\<host>\share.
            if i + 1 < t.len() && t[i + 1] == '\\' {
                let mut k = i + 2;
                let host0 = k;
                while k < t.len() && t[k] != '\\' && is_path_char(t[k]) {
                    k += 1;
                }
                if k > host0 && k < t.len() && t[k] == '\\' {
                    out.push_str("\\\\<host>\\");
                    i = k + 1;
                    continue;
                }
            }
            if let Some(mut k) = starts_with_ci(&t, i, "\\users\\") {
                out.push_str("\\Users\\<user>");
                while k < t.len() && t[k] != '\\' && is_path_char(t[k]) {
                    k += 1;
                }
                i = k;
                continue;
            }
            if let Some(mut k) = starts_with_ci(&t, i, "\\documents and settings\\") {
                out.push_str("\\Documents and Settings\\<user>");
                while k < t.len() && t[k] != '\\' && is_path_char(t[k]) {
                    k += 1;
                }
                i = k;
                continue;
            }
        }
        out.push(t[i]);
        i += 1;
    }
    out
}

/// Keyed identifiers: the label is preserved, the value is masked with a
/// class placeholder. Only keys whose values are known identifiers are listed,
/// so unrelated prose is untouched.
fn key_placeholder(key: &str) -> Option<&'static str> {
    match key.to_ascii_lowercase().as_str() {
        "ssid" | "network" => Some("<ssid>"),
        "user" | "username" | "owner" | "account" | "login" => Some("<user>"),
        "host" | "hostname" | "computer" | "machine" | "server" => Some("<host>"),
        "path" | "file" | "dir" | "directory" | "filename" => Some("<path>"),
        "session" | "label" | "note" | "detail" | "message" | "description" | "command" | "cmd"
        | "args" | "arguments" => Some("<redacted>"),
        "device" | "devicename" | "device_name" | "adapter" | "adapter_name" | "descr" | "name"
        | "display" | "monitor" | "gpu" | "manufacturer" | "model" => Some("<redacted>"),
        _ => None,
    }
}

/// Keys whose bare (unquoted) value is free text and may span spaces; those
/// consume to end of line rather than stopping at the first whitespace.
fn is_text_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "session"
            | "label"
            | "note"
            | "detail"
            | "message"
            | "description"
            | "command"
            | "cmd"
            | "args"
            | "arguments"
    )
}

/// Dotted metric labels (e.g. `gpu.adapter.util_total_pct`) are labels, not
/// identifiers, so a keyed `name` value that looks like one is preserved.
fn is_field_label(value: &str) -> bool {
    !value.is_empty()
        && !value.chars().any(|c| c.is_whitespace())
        && value.matches('.').count() >= 2
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

fn mask_keyed_identifiers(text: &str) -> String {
    let t: Vec<char> = text.chars().collect();
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let is_value_end =
        |c: char| c.is_whitespace() || matches!(c, ',' | ';' | ')' | ']' | '}' | '<' | '>');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < t.len() {
        if is_word(t[i]) && (i == 0 || !is_word(t[i - 1])) {
            let mut j = i;
            while j < t.len() && is_word(t[j]) {
                j += 1;
            }
            let key: String = t[i..j].iter().collect();
            if let Some(placeholder) = key_placeholder(&key) {
                let mut k = j;
                while k < t.len() && t[k] == ' ' {
                    k += 1;
                }
                if k < t.len() && (t[k] == ':' || t[k] == '=') {
                    k += 1;
                    while k < t.len() && t[k] == ' ' {
                        k += 1;
                    }
                    if k < t.len() && t[k] == '"' {
                        // Quoted value: keep the quotes, mask the contents.
                        let open = k;
                        let mut c = open + 1;
                        let mut inner = String::new();
                        while c < t.len() && t[c] != '"' {
                            if t[c] == '\\' && c + 1 < t.len() {
                                inner.push(t[c]);
                                c += 1;
                            }
                            inner.push(t[c]);
                            c += 1;
                        }
                        out.push_str(&t[i..=open].iter().collect::<String>());
                        if keep_value(&key, placeholder, &inner) {
                            out.push_str(&inner);
                        } else {
                            out.push_str(placeholder);
                        }
                        if c < t.len() {
                            out.push('"');
                            i = c + 1;
                        } else {
                            i = c;
                        }
                        continue;
                    }
                    // Bare value: free-text keys consume to end of line so a
                    // note/detail cannot leak the rest of its sentence; other
                    // keys stop at the next delimiter.
                    let d0 = k;
                    if is_text_key(&key) {
                        while k < t.len() && t[k] != '\n' && t[k] != '\r' {
                            k += 1;
                        }
                    } else {
                        while k < t.len() && !is_value_end(t[k]) {
                            k += 1;
                        }
                    }
                    if k > d0 {
                        let value: String = t[d0..k].iter().collect();
                        out.push_str(&t[i..d0].iter().collect::<String>());
                        if keep_value(&key, placeholder, &value) {
                            out.push_str(&value);
                        } else {
                            out.push_str(placeholder);
                        }
                        i = k;
                        continue;
                    }
                }
            }
        }
        out.push(t[i]);
        i += 1;
    }
    out
}

/// Value already a placeholder (idempotence) or a preserved field label.
fn keep_value(key: &str, _placeholder: &str, value: &str) -> bool {
    if value.starts_with('<') {
        return true;
    }
    let name_like = matches!(
        key.to_ascii_lowercase().as_str(),
        "name" | "device" | "devicename" | "device_name" | "adapter" | "adapter_name" | "descr"
    );
    name_like && is_field_label(value)
}

/// Mask GUID tokens: 36-char hex+dash (4 dashes) or 32-char hex, as `<guid>`.
fn mask_guids(text: &str) -> String {
    let is_hex = |c: char| c.is_ascii_hexdigit();
    let t: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < t.len() {
        let run_start = is_hex(t[i]) && (i == 0 || !(is_hex(t[i - 1]) || t[i - 1] == '-'));
        if run_start {
            let start = i;
            let mut j = i;
            while j < t.len() && (is_hex(t[j]) || t[j] == '-') {
                j += 1;
            }
            let len = j - start;
            let dashes = t[start..j].iter().filter(|c| **c == '-').count();
            let boundary_after = j == t.len() || !(is_hex(t[j]) || t[j] == '-');
            if boundary_after && ((len == 36 && dashes == 4) || (len == 32 && dashes == 0)) {
                out.push_str("<guid>");
                i = j;
                continue;
            }
        }
        out.push(t[i]);
        i += 1;
    }
    out
}

/// Mask `pid`/`ppid` labels followed by a number in any common form
/// (`pid 123`, `PID=123`, `"pid":123`, `ppid: 123`). The label is preserved.
fn mask_pids(text: &str) -> String {
    let t: Vec<char> = text.chars().collect();
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < t.len() {
        let mut label_len = 0;
        if i == 0 || !is_word(t[i - 1]) {
            for m in ["ppid", "pid"] {
                if starts_with_ci(&t, i, m).is_some() {
                    label_len = m.len();
                    break;
                }
            }
        }
        if label_len > 0 {
            let mut k = i + label_len;
            while k < t.len() && matches!(t[k], ' ' | '\t' | '=' | ':' | '#' | '"' | '\'') {
                k += 1;
            }
            let d0 = k;
            while k < t.len() && t[k].is_ascii_digit() {
                k += 1;
            }
            if k > d0 {
                out.push_str(&t[i..d0].iter().collect::<String>());
                out.push_str("<pid>");
                i = k;
                continue;
            }
        }
        out.push(t[i]);
        i += 1;
    }
    out
}

/// Mask MAC addresses (`AA:BB:CC:DD:EE:FF` or dash-separated) as `<mac>`.
fn mask_mac(text: &str) -> String {
    let is_hex = |c: char| c.is_ascii_hexdigit();
    let t: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < t.len() {
        let run_start =
            is_hex(t[i]) && (i == 0 || !(is_hex(t[i - 1]) || t[i - 1] == ':' || t[i - 1] == '-'));
        if run_start && i + 2 < t.len() && is_hex(t[i + 1]) {
            let sep = t[i + 2];
            if sep == ':' || sep == '-' {
                let mut p = i;
                let mut groups = 0;
                loop {
                    if p + 1 >= t.len() || !is_hex(t[p]) || !is_hex(t[p + 1]) {
                        break;
                    }
                    groups += 1;
                    p += 2;
                    if p >= t.len() || t[p] != sep {
                        break;
                    }
                    p += 1;
                }
                let boundary_after = p == t.len() || !(is_hex(t[p]) || t[p] == ':' || t[p] == '-');
                if groups == 6 && boundary_after {
                    out.push_str("<mac>");
                    i = p;
                    continue;
                }
            }
        }
        out.push(t[i]);
        i += 1;
    }
    out
}

/// Mask dotted-quad IPv4 addresses as `<ip>`. Four groups, each 0..=255, so
/// long version/build numbers are left intact.
fn mask_ipv4(text: &str) -> String {
    let t: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < t.len() {
        if t[i].is_ascii_digit() && (i == 0 || !t[i - 1].is_ascii_digit()) {
            let mut p = i;
            let mut groups = 0;
            let ok = loop {
                let g0 = p;
                while p < t.len() && t[p].is_ascii_digit() {
                    p += 1;
                }
                let len = p - g0;
                if len == 0 || len > 3 {
                    break false;
                }
                let val: u32 = t[g0..p].iter().collect::<String>().parse().unwrap_or(999);
                if val > 255 {
                    break false;
                }
                groups += 1;
                if groups == 4 {
                    break true;
                }
                if p < t.len() && t[p] == '.' {
                    p += 1;
                } else {
                    break false;
                }
            };
            let boundary_after = p == t.len() || !(t[p].is_ascii_digit() || t[p] == '.');
            if ok && boundary_after {
                out.push_str("<ip>");
                i = p;
                continue;
            }
        }
        out.push(t[i]);
        i += 1;
    }
    out
}

/// Idle-like subset: discharge points whose nearest CPU sample (<=2.5s)
/// shows <10% utility and whose nearest GPU sample shows <0.5% total.
/// Missing CPU or GPU evidence is UNKNOWN, not idle: a sample without GPU
/// corroboration is excluded rather than assumed quiet.
pub fn idle_subset(s: &SessionData) -> Vec<f64> {
    let near = |t: f64, series: &[(f64, Option<f64>)]| -> Option<f64> {
        series
            .iter()
            .min_by(|a, b| (a.0 - t).abs().total_cmp(&(b.0 - t).abs()))
            .filter(|(st, _)| (st - t).abs() <= 2.5)
            .and_then(|(_, v)| *v)
    };
    let cpu_u: Vec<(f64, Option<f64>)> = s.cpu.iter().map(|p| (p.t, p.utility.val())).collect();
    let gpu_t: Vec<(f64, Option<f64>)> = s
        .gpu
        .iter()
        .map(|g| {
            let present: Vec<f64> = g
                .adapters
                .iter()
                .filter_map(|a| a.total_util.val())
                .collect();
            let max = present.into_iter().reduce(f64::max);
            (g.t, max)
        })
        .collect();
    s.battery
        .iter()
        .filter(|p| p.discharge.val().is_some())
        .filter(|p| {
            near(p.t, &cpu_u).map(|u| u < 10.0).unwrap_or(false)
                && near(p.t, &gpu_t).map(|g| g < 0.5).unwrap_or(false)
        })
        .filter_map(|p| p.discharge.val())
        .collect()
}

pub fn render_report(s: &SessionData, d: &Diagnosis) -> String {
    let mut o = "POWER FORENSICS REPORT\n".to_string();
    o.push_str(&format!("session: {}\n\n", s.label));
    // Battery (measured vs unavailable, never invented).
    o.push_str("Battery [measured unless noted]\n");
    let multi = s.battery_meta.battery_count.is_some_and(|c| c > 1);
    let basis = match s.battery_meta.battery_index {
        Some(i) if multi => format!(
            "battery #{i} (of {} detected)",
            s.battery_meta.battery_count.unwrap_or(0)
        ),
        _ => "battery".to_string(),
    };
    if multi {
        o.push_str(
            "  note: multiple batteries detected; capacity is reported per physical battery and\n  \
             dynamic state is the Windows aggregate — the two are never merged into one health/SoC claim [derived]\n",
        );
    }
    // Battery health uses same-basis pairing only (see battery::headline_health):
    // multi-battery systems never pair the aggregate full-charge with one
    // battery's design; the headline is unavailable with a pointer instead.
    let hh = headline_health(&s.battery_meta);
    if multi {
        o.push_str(&format!(
            "  {basis}: health [unavailable] ({})\n",
            hh.reason.as_deref().unwrap_or("insufficient capacity data")
        ));
    } else {
        match (s.battery_meta.full_mwh, s.battery_meta.design_mwh) {
            (Some(f), Some(dd)) => {
                // Same-basis single battery: headline ratio comes from the
                // checked helper, never from a cross-basis pairing.
                let pct = hh
                    .value
                    .map(|h| format!("{:.1}%", h * 100.0))
                    .unwrap_or_else(|| "[unavailable]".to_string());
                o.push_str(&format!(
                    "  {basis}: full charge {:.1} Wh, design {:.1} Wh, health {pct}\n",
                    f / 1000.0,
                    dd / 1000.0,
                ));
            }
            (Some(f), None) => o.push_str(&format!(
                "  {basis}: full charge {:.1} Wh, design: unknown (driver exposes none) -> health [unavailable]\n",
                f / 1000.0
            )),
            _ => o.push_str("  capacity data [unavailable]\n"),
        }
    }
    if s.battery_meta.batteries.len() > 1 {
        o.push_str("  physical batteries:\n");
        for b in &s.battery_meta.batteries {
            let wh = |v: Option<f64>| {
                v.map(|x| format!("{:.1} Wh", x / 1000.0))
                    .unwrap_or_else(|| "[unknown]".to_string())
            };
            let health = per_battery_health(b.full_mwh, b.designed_mwh)
                .map(|h| format!("{:.1}%", h * 100.0))
                .unwrap_or_else(|| "[unavailable]".to_string());
            o.push_str(&format!(
                "    #{}: design {}, full {}, health {}, cycles {}, chemistry {}, id {}\n",
                b.index,
                wh(b.designed_mwh),
                wh(b.full_mwh),
                health,
                b.cycle_count
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "[unknown]".to_string()),
                b.chemistry.as_deref().unwrap_or("[unknown]"),
                b.unique_id.as_deref().unwrap_or("[unknown]"),
            ));
        }
    }
    // Idle.
    let idle = idle_subset(s);
    o.push_str("Idle (discharge with CPU<10%, GPU<0.5%) [derived]\n");
    match PowerSummary::of(&idle) {
        Some(sum) => {
            o.push_str(&format!(
                "  n={} median {:.2} W p10 {:.2} p90 {:.2}\n",
                sum.n, sum.median, sum.p10, sum.p90
            ));
            if let Some(rem) = s
                .battery
                .iter()
                .filter_map(|p| p.remaining_wh.val())
                .next_back()
                && let Some(h) = stats::runtime_hours(rem, sum.median)
            {
                o.push_str(&format!("  estimated runtime at idle draw: {}\n", fmt_h(h)));
            }
        }
        None => o.push_str("  no idle-like periods observed\n"),
    }
    // Issues from the diagnosis engine.
    o.push_str("\nDetected issues (correlation with confidence, not proof)\n");
    if d.evidence.is_empty() {
        o.push_str("  none\n");
    }
    for (i, e) in d.evidence.iter().enumerate() {
        o.push_str(&format!(
            "  {}. {} [confidence {}]\n     {}\n",
            i + 1,
            e.label,
            e.confidence.as_str(),
            e.detail
        ));
    }
    if let Some(u) = d.unattributed_w {
        o.push_str(&format!(
            "  unattributed change: {u:+.2} W [derived remainder]\n"
        ));
    }
    // Configuration.
    o.push_str("\nConfiguration [measured]\n");
    o.push_str(&format!(
        "  scheme: {}\n",
        s.policy.scheme_name.as_deref().unwrap_or("unknown")
    ));
    let pol = |label: &str, v: Option<f64>, unit: &str| {
        format!(
            "  {label}: {}\n",
            v.map(|x| format!("{x:.0}{unit}"))
                .unwrap_or("[unavailable]".to_string())
        )
    };
    o.push_str(&pol("cpu_min_dc", s.policy.cpu_min_dc.val(), "%"));
    o.push_str(&pol("cpu_max_dc", s.policy.cpu_max_dc.val(), "%"));
    o.push_str(&pol("brightness_dc", s.policy.brightness_dc.val(), "%"));
    o.push_str(&pol(
        "display_timeout_dc",
        s.policy.display_timeout_dc.val(),
        "s",
    ));
    o.push_str(&pol(
        "sleep_timeout_dc",
        s.policy.sleep_timeout_dc.val(),
        "s",
    ));
    // Energy footers.
    if let Some(f) = &s.footer {
        let wh = |k: &str| f.get(k).and_then(|v| v.num());
        o.push_str("\nSession energy [derived]\n");
        o.push_str(&format!(
            "  discharge: {} charge: {}\n",
            wh("discharge_wh")
                .map(|x| format!("{x:.3} Wh"))
                .unwrap_or("[unavailable]".to_string()),
            wh("charge_wh")
                .map(|x| format!("{x:.3} Wh"))
                .unwrap_or("[unavailable]".to_string()),
        ));
        if let Some(pct) = wh("discharge_coverage_pct") {
            let unknown = wh("discharge_unknown_s").unwrap_or(0.0);
            o.push_str(&format!(
                "  telemetry coverage: {pct:.1}% ({} unobserved across AC/charge/sleep/gaps)\n",
                fmt_duration(unknown)
            ));
        }
    }
    if let Some(f) = discharge_freshness(s) {
        o.push_str(&format!(
            "  discharge freshness [derived]: sample age {:.1}s, value age {:.1}s, {} distinct updates{}\n",
            f.sample_age_s,
            f.value_age_s,
            f.updates,
            f.observed_hz
                .map(|hz| format!(", ~{hz:.2} Hz"))
                .unwrap_or_default()
        ));
    }
    o.push_str("\nMethod: measurements, derivations, and estimates are tagged inline; unattributed remainder is explicit.\n");
    o
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    use crate::session::{
        BatteryMeta, BatteryPoint, CpuPoint, DisplayPoint, GpuAdapterPoint, GpuPoint, Policy,
        ProcEntry, ProcPoint,
    };
    use crate::telemetry::{ClockStamp, Telemetry};

    fn ts() -> ClockStamp {
        ClockStamp {
            wall_millis: 0,
            mono_millis: 0,
        }
    }

    fn step_session() -> SessionData {
        // 20 ticks at 5.7 W, then 20 at 11.4 W with GPU + C3 + proc changes.
        let mut s = SessionData {
            label: "synth".to_string(),
            battery_meta: BatteryMeta {
                full_mwh: Some(38700.0),
                design_mwh: None,
                battery_index: Some(0),
                battery_count: Some(1),
                batteries: Vec::new(),
                ..Default::default()
            },
            policy: Policy {
                scheme_name: Some("Balanced".to_string()),
                cpu_min_dc: Telemetry::measured(5.0, "os", ts()),
                ..Default::default()
            },
            ..Default::default()
        };
        for i in 0..40 {
            let high = i >= 20;
            let t = i as f64;
            s.battery.push(BatteryPoint {
                t,
                discharge: Telemetry::measured(if high { 11.4 } else { 5.7 }, "battery", ts()),
                remaining_wh: Telemetry::measured(30.0 - t * 0.002, "battery", ts()),
                ..Default::default()
            });
            s.cpu.push(CpuPoint {
                t,
                utility: Telemetry::measured(if high { 18.0 } else { 4.0 }, "cpu", ts()),
                c3_pct: Telemetry::measured(if high { 40.0 } else { 92.0 }, "cpu", ts()),
                pkg_derived_w: Telemetry::estimated(if high { 4.2 } else { 1.1 }, "cpu", ts()),
                ..Default::default()
            });
            s.gpu.push(GpuPoint {
                t,
                adapters: vec![GpuAdapterPoint {
                    name: "NVIDIA".to_string(),
                    discrete: true,
                    total_util: Telemetry::measured(if high { 6.0 } else { 0.0 }, "gpu", ts()),
                    ..Default::default()
                }],
                ..Default::default()
            });
            let mut top = vec![ProcEntry {
                pid: 100,
                ppid: 1,
                name: "idle.exe".to_string(),
                start_unix_ms: Some(1000),
                exit_unix_ms: None,
                cpu: Telemetry::derived(0.1, "proc", ts()),
            }];
            if high {
                top.push(ProcEntry {
                    pid: 200,
                    ppid: 1,
                    name: "game.exe".to_string(),
                    start_unix_ms: Some(2000),
                    exit_unix_ms: None,
                    cpu: Telemetry::derived(12.0, "proc", ts()),
                });
                // Start time inside the after-window (wall_at(20)=20000).
                top.push(ProcEntry {
                    pid: 300,
                    ppid: 1,
                    name: "newgame.exe".to_string(),
                    start_unix_ms: Some(30000),
                    exit_unix_ms: None,
                    cpu: Telemetry::derived(3.0, "proc", ts()),
                });
            }
            s.procs.push(ProcPoint {
                t,
                top,
                ..Default::default()
            });
            s.display.push(DisplayPoint {
                t,
                brightness: Telemetry::measured(40.0, "display", ts()),
                ..Default::default()
            });
        }
        s
    }

    #[test]
    fn diagnose_attributes_gpu_step() {
        let s = step_session();
        let d = diagnose_session(&s, Some(5.6));
        let step = d.step.as_ref().unwrap();
        assert!((step.delta_w - 5.7).abs() < 0.3);
        assert!((d.baseline_abs_w.unwrap() - (d.median_w.unwrap() - 5.6)).abs() < 1e-9);
        let labels: Vec<&str> = d.evidence.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.iter().any(|l| l.contains("GPU activity (NVIDIA)")));
        assert!(labels.iter().any(|l| l.contains("C3 residency decreased")));
        assert!(labels.iter().any(|l| l.contains("game.exe")));
        // Lifecycle language: a real start vs merely becoming active.
        assert!(
            labels
                .iter()
                .any(|l| l.starts_with("Process started newgame.exe"))
        );
        assert!(labels.iter().any(|l| l.contains("became active")));
        assert!(!labels.iter().any(|l| l.contains("New process")));
        // Engine-% evidence carries no watts; the estimate does.
        let gpu_ev = d
            .evidence
            .iter()
            .find(|e| e.label.starts_with("GPU activity"))
            .unwrap();
        assert_eq!(gpu_ev.delta_w, None);
        assert_eq!(gpu_ev.confidence, Confidence::High);
        // The power-sensor status rides alongside, unavailable with reasons.
        let sensor_ev = d
            .evidence
            .iter()
            .find(|e| e.label == "GPU power sensor")
            .unwrap();
        assert!(sensor_ev.detail.contains("unavailable"));
        assert!(sensor_ev.detail.contains("NVML"));
        let pkg_ev = d
            .evidence
            .iter()
            .find(|e| e.label.contains("package"))
            .unwrap();
        assert!(pkg_ev.delta_w.is_some());
        assert_eq!(pkg_ev.confidence, Confidence::Low);
        // Full step delta is unattributed (no watt sensors).
        assert!((d.unattributed_w.unwrap() - step.delta_w).abs() < 1e-12);
        let text = render_diagnosis("synth", &d);
        assert!(text.contains("DIAGNOSIS"));
        assert!(text.contains("[derived]"));
        assert!(text.contains("correlation, not proven causation"));
    }

    #[test]
    fn diagnose_empty_is_honest() {
        let s = SessionData::default();
        let d = diagnose_session(&s, None);
        assert!(d.median_w.is_none());
        assert!(d.step.is_none());
        assert!(d.note.contains("no battery discharge"));
    }

    #[test]
    fn aggregate_summaries_are_labeled_derived_not_measured() {
        // A median/integral is a derivation over measured samples; the raw
        // acquisition provenance (measured) must not be echoed onto it.
        let s = step_session();
        let d = diagnose_session(&s, None);
        let text = render_diagnosis("synth", &d);
        assert!(text.contains("[derived] discharge median"), "{text}");
        assert!(!text.contains("[measured] discharge median"), "{text}");
        // Comparison aggregates likewise.
        let c = compare_sessions(&s, &s);
        let text = render_comparison(&c);
        assert!(
            !text.contains("[measured] discharge median")
                && !text.contains("[measured] session discharge energy")
                && !text.contains("Session energy [measured]"),
            "{text}"
        );
    }

    #[test]
    fn baseline_roundtrip() {
        let s = step_session();
        let b = baseline_of("base", &s).unwrap();
        assert!((b.median - 8.55).abs() < 1e-9); // median of 20x5.7 + 20x11.4
        let j = baseline_to_json(&b);
        let back = baseline_from_json(&crate::json::parse(&j).unwrap()).unwrap();
        assert_eq!(back.label, "base");
        assert!((back.median - b.median).abs() < 1e-9);
        assert!(baseline_from_json(&crate::json::parse("{\"a\":1}").unwrap()).is_none());
    }

    fn cq(n: usize, dur: f64, uniq: usize, spread: f64) -> CaptureQuality {
        CaptureQuality {
            n,
            duration_secs: dur,
            unique_values: uniq,
            spread,
            abs_spread_w: 0.1,
            max_gap_secs: 1.0,
            coverage: Some(1.0),
            rho_lag1: 0.0,
            observed_hz: (dur > 0.0 && n > 1).then(|| (n as f64 - 1.0) / dur),
            median_dt_secs: if n > 1 { dur / (n as f64 - 1.0) } else { 0.0 },
            discontinuities: 0,
            unobserved_secs: 0.0,
        }
    }

    #[test]
    fn confidence_rule() {
        // Long, stable, many distinct values is the only path to High.
        let (lvl, _, cf) = ab_confidence(
            &cq(60, 120.0, 50, 0.1),
            &cq(60, 120.0, 50, 0.2),
            Some(2.0),
            None,
        );
        assert_eq!(lvl, Confidence::High);
        assert!(cf.is_empty());
        // 20 rows in five seconds must NOT become High (the plan's gate).
        assert_ne!(
            ab_confidence(
                &cq(20, 5.0, 20, 0.1),
                &cq(20, 5.0, 20, 0.1),
                Some(2.0),
                None
            )
            .0,
            Confidence::High
        );
        // Repeated cached values cap effective n.
        assert_eq!(
            ab_confidence(
                &cq(30, 120.0, 3, 0.1),
                &cq(30, 120.0, 3, 0.1),
                Some(2.0),
                None
            )
            .0,
            Confidence::Low
        );
        // Unstable spread downgrades.
        assert_eq!(
            ab_confidence(
                &cq(30, 120.0, 30, 0.9),
                &cq(30, 120.0, 30, 0.1),
                Some(2.0),
                None
            )
            .0,
            Confidence::Low
        );
        // An effect within the measured spread is a confounder, not evidence.
        let (lvl, _, cf) = ab_confidence(
            &cq(60, 120.0, 50, 0.1),
            &cq(60, 120.0, 50, 0.1),
            Some(0.05),
            None,
        );
        assert_eq!(lvl, Confidence::Medium);
        assert!(cf.iter().any(|c| c.contains("within measured spread")));
    }

    #[test]
    fn discharge_freshness_separates_sample_and_value_age() {
        let mut s = SessionData::default();
        for (t, w) in [(0.0, 5.0), (2.0, 6.0), (4.0, 6.0), (6.0, 6.0), (10.0, 6.0)] {
            s.battery.push(BatteryPoint {
                t,
                discharge: Telemetry::measured(w, "battery", ts()),
                ..Default::default()
            });
        }
        // A later non-discharge sample makes the last discharge sample older.
        s.battery.push(BatteryPoint {
            t: 12.0,
            charge: Telemetry::measured(4.0, "battery", ts()),
            ..Default::default()
        });
        let f = discharge_freshness(&s).unwrap();
        assert!((f.sample_age_s - 2.0).abs() < 1e-9);
        assert!((f.value_age_s - 8.0).abs() < 1e-9);
        assert_eq!(f.updates, 2);
    }

    #[test]
    fn comparison_validity_gates_thin_captures() {
        let a = step_session();
        let mut b = step_session();
        b.label = "b".to_string();
        // Thin capture: two discharge samples over one second.
        b.battery.clear();
        for (t, w) in [(0.0, 6.0), (1.0, 6.0)] {
            b.battery.push(BatteryPoint {
                t,
                discharge: Telemetry::measured(w, "battery", ts()),
                remaining_wh: Telemetry::measured(20.0, "battery", ts()),
                ..Default::default()
            });
        }
        assert_eq!(compare_sessions(&a, &b).validity, Validity::Invalid);
        // No discharge at all on one side is invalid.
        let e = SessionData {
            label: "empty".to_string(),
            ..Default::default()
        };
        assert_eq!(compare_sessions(&a, &e).validity, Validity::Invalid);
    }

    #[test]
    fn redaction_masks_names_pids_guids() {
        let text = "Process started chrome.exe (pid 4321)\n\
                    GUID 54533251-82be-4824-96c1-47b60b740d00\n";
        let r = redact_report(text, &["chrome.exe".to_string()]);
        assert!(!r.contains("chrome.exe"));
        assert!(r.contains("process-1"));
        assert!(!r.contains("4321"));
        assert!(!r.contains("54533251-82be-4824-96c1-47b60b740d00"));
        assert!(r.contains("<guid>"));
    }

    #[test]
    fn redaction_covers_session_identifiers_and_is_idempotent() {
        // Realistic slice of a session report/export: identity in notes,
        // event details, Wi-Fi SSID, adapter/device names, profile paths,
        // UNC share, usernames, host, IP/MAC, and every PID spelling.
        let text = r#"DASHBOARD DESKTOP-ABC123
TIMELINE DESKTOP-ABC123  t=0s..9s  max=12.30W
session: DESKTOP-ABC123
note: borrowed from alice on floor 3
detail: connected to HomeNet at 01:23:45
ssid: HomeNet
user: alice
hostname: DESKTOP-ABC123
path: C:\Users\alice\Documents\draft.txt
share: \\DESKTOP-ABC123\private
adapter: Intel(R) Wi-Fi 6 AX201 160MHz
device: Logitech USB Optical Mouse
gpu: NVIDIA GeForce RTX 4070 Laptop GPU
peer 192.168.1.55 mac AA:BB:CC:DD:EE:FF
chrome.exe (pid 4321) PID=999 "pid":1234 ppid 42
GUID 54533251-82be-4824-96c1-47b60b740d00 and 5453325182be482496c147b60b740d00
short proc cu.exe
"#;
        let names = vec!["chrome.exe".to_string(), "cu.exe".to_string()];
        let r = redact_report(text, &names);
        // No identifier survives in any form.
        for secret in [
            "DESKTOP-ABC123",
            "HomeNet",
            "alice",
            r"C:\Users",
            "Intel(R) Wi-Fi 6 AX201",
            "Logitech USB Optical Mouse",
            "NVIDIA GeForce RTX 4070",
            "192.168.1.55",
            "AA:BB:CC:DD:EE:FF",
            "chrome.exe",
            "cu.exe",
            "54533251-82be-4824-96c1-47b60b740d00",
            "5453325182be482496c147b60b740d00",
        ] {
            assert!(!r.contains(secret), "leaked {secret:?} in:\n{r}");
        }
        // PID digits are gone while the labels stay.
        assert!(!r.contains("pid 4321") && !r.contains("PID=999"), "{r}");
        assert!(!r.contains(":1234") && !r.contains("ppid 42"), "{r}");
        assert!(r.contains("pid <pid>") && r.contains("PID=<pid>"), "{r}");
        // Class placeholders are consistent, and labels/structure survive.
        assert!(
            r.contains("<ssid>") && r.contains("<user>") && r.contains("<host>"),
            "{r}"
        );
        assert!(
            r.contains("<path>")
                && r.contains("<guid>")
                && r.contains("<mac>")
                && r.contains("<ip>"),
            "{r}"
        );
        assert!(r.contains("process-1") && r.contains("process-2"), "{r}");
        assert!(r.contains("ppid <pid>"), "{r}");
        // Deterministic and idempotent: re-redacting changes nothing.
        assert_eq!(
            r,
            redact_report(text, &names),
            "redaction is not deterministic"
        );
        assert_eq!(r, redact_report(&r, &names), "redaction is not idempotent");
    }

    #[test]
    fn compare_math_and_render() {
        let a = step_session();
        let mut b = step_session();
        b.label = "after".to_string();
        for p in &mut b.battery {
            if let Some(w) = p.discharge.val() {
                let stamp = p.discharge.stamp;
                p.discharge = Telemetry::measured(w - 3.5, p.discharge.source, stamp);
            }
        }
        let c = compare_sessions(&a, &b);
        assert!((c.delta_w.unwrap() + 3.5).abs() < 1e-9);
        assert!(c.rel.unwrap() < 0.0);
        assert!(c.runtime_b_h.unwrap() > c.runtime_a_h.unwrap());
        assert!(c.lines.iter().any(|l| l.contains("CPU utility median")));
        let text = render_comparison(&c);
        assert!(text.contains("A/B COMPARISON"));
        assert!(text.contains("confidence:"));
    }

    #[test]
    fn sleep_gap_reduces_discharge_coverage() {
        let mut s = SessionData {
            label: "gap".to_string(),
            ..Default::default()
        };
        for t in [0.0, 1.0, 2.0, 602.0, 603.0, 604.0] {
            s.battery.push(BatteryPoint {
                t,
                discharge: Telemetry::measured(6.0, "battery", ts()),
                ..Default::default()
            });
        }
        let vals: Vec<f64> = s.battery.iter().filter_map(|p| p.discharge.val()).collect();
        let q = capture_quality(&s, &vals);
        // The 600 s sleep gap must be excluded, so coverage collapses.
        assert!(q.coverage.unwrap() < 0.5, "coverage {:?}", q.coverage);
        assert!(q.max_gap_secs > 100.0);
    }

    #[test]
    fn wall_step_breaks_discharge_coverage_without_mono_gap() {
        // Mono keeps a 1 s cadence (below the cadence gap), but wall jumps
        // 600 s: the both-clock scan must still force a boundary and report
        // the unobserved interval. Mono-only integration would miss it.
        let mut s = SessionData::default();
        for (t, wall) in [
            (0.0, 1_000_000u64),
            (1.0, 1_001_000),
            (2.0, 1_601_000),
            (3.0, 1_602_000),
        ] {
            s.battery.push(BatteryPoint {
                t,
                discharge: Telemetry::measured(
                    6.0,
                    "battery",
                    ClockStamp {
                        wall_millis: wall,
                        mono_millis: (t * 1000.0) as u64,
                    },
                ),
                ..Default::default()
            });
        }
        let vals: Vec<f64> = s.battery.iter().filter_map(|p| p.discharge.val()).collect();
        let q = capture_quality(&s, &vals);
        assert!(q.discontinuities >= 1, "no boundary forced: {q:?}");
        assert!(q.unobserved_secs >= 600.0, "unobserved not reported: {q:?}");
    }

    #[test]
    fn settling_trim_and_block_aggregation() {
        let s = step_session(); // 40 battery points at t = 0..39
        let trimmed = trim_settle(&s, 10.0);
        assert!(trimmed.battery.iter().all(|p| p.t >= 10.0));
        assert_eq!(trimmed.battery.len(), 30);
        let q1 = block_median(&trimmed).1;
        let agg = aggregate_quality(&[q1, q1]);
        assert_eq!(agg.n, q1.n * 2);
        assert!((agg.duration_secs - q1.duration_secs * 2.0).abs() < 1e-9);
    }

    #[test]
    fn report_marks_unknown_design() {
        let s = step_session();
        let d = diagnose_session(&s, None);
        let r = render_report(&s, &d);
        assert!(r.contains("POWER FORENSICS REPORT"));
        assert!(r.contains("design: unknown"));
        assert!(r.contains("[unavailable]"));
        assert!(r.contains("Idle (discharge with CPU<10%"));
        // First-half ticks are idle-like (4% CPU, 0 GPU).
        assert!(r.contains("estimated runtime at idle draw"));
    }

    #[test]
    fn time_median_windows() {
        let series = vec![(0.0, 1.0), (1.0, 2.0), (2.0, 100.0)];
        assert_eq!(time_median(&series, 0.0, 2.0), Some(1.5));
        assert_eq!(time_median(&series, 5.0, 9.0), None);
    }

    fn load_fixture(name: &str) -> SessionData {
        let text: &str = match name {
            "discharge" => include_str!("../fixtures/discharge.jsonl"),
            "charging" => include_str!("../fixtures/charging.jsonl"),
            "churn" => include_str!("../fixtures/process-churn.jsonl"),
            _ => panic!("unknown fixture"),
        };
        crate::session::load_session(name, text).unwrap()
    }

    #[test]
    fn fixture_steady_discharge_has_no_step() {
        let s = load_fixture("discharge");
        let d = diagnose_session(&s, None);
        assert_eq!(d.median_w, Some(5.7));
        assert!(d.step.is_none());
        assert!(d.note.contains("stable"));
        let b = baseline_of("discharge", &s).unwrap();
        assert!((b.median - 5.7).abs() < 1e-9);
        assert!((b.p10 - 5.7).abs() < 1e-9);
    }

    #[test]
    fn fixture_charging_yields_no_diagnosis() {
        let s = load_fixture("charging");
        let d = diagnose_session(&s, None);
        assert!(d.median_w.is_none());
        assert!(baseline_of("charging", &s).is_none());
    }

    #[test]
    fn fixture_compare_self_is_zero() {
        let a = load_fixture("discharge");
        let b = load_fixture("discharge");
        let c = compare_sessions(&a, &b);
        assert_eq!(c.delta_w, Some(0.0));
        assert_eq!(c.rel, Some(0.0));
    }

    #[test]
    fn normalized_runtime_shares_one_energy_basis() {
        let a = step_session();
        let mut b = step_session();
        b.label = "after".to_string();
        for p in &mut b.battery {
            if let Some(w) = p.discharge.val() {
                let stamp = p.discharge.stamp;
                p.discharge = Telemetry::measured(w - 3.5, p.discharge.source, stamp);
            }
        }
        // full_mwh = 38700 mWh = 38.7 Wh on both sides (set in step_session).
        let c = compare_sessions(&a, &b);
        let na = c.runtime_norm_a_h.unwrap();
        let nb = c.runtime_norm_b_h.unwrap();
        assert!((na - 38.7 / 8.55).abs() < 1e-9);
        assert!(nb > na);
        assert!((c.projected_delta_min.unwrap() - (nb - na) * 60.0).abs() < 1e-9);
        // Old own-remaining projections are kept for compat.
        assert!(c.runtime_a_h.is_some() && c.runtime_b_h.is_some());
        let text = render_comparison(&c);
        assert!(text.contains("normalized full-charge runtime"));
    }

    #[test]
    fn multi_battery_headline_never_pairs_aggregate() {
        use crate::session::BatteryStaticEntry;
        let mut s = step_session();
        s.battery_meta.battery_count = Some(2);
        s.battery_meta.full_mwh = Some(80000.0);
        s.battery_meta.design_mwh = Some(42000.0);
        s.battery_meta.batteries = vec![
            BatteryStaticEntry {
                index: 0,
                designed_mwh: Some(42000.0),
                full_mwh: Some(38700.0),
                ..Default::default()
            },
            BatteryStaticEntry {
                index: 1,
                designed_mwh: Some(40000.0),
                full_mwh: Some(38000.0),
                ..Default::default()
            },
        ];
        let d = diagnose_session(&s, None);
        let r = render_report(&s, &d);
        assert!(r.contains("multi-battery: see per-battery"), "{r}");
        assert!(!r.contains("health 190"), "{r}"); // 80000/42000 must never appear
        assert!(r.contains("92.1%"), "{r}"); // 38700/42000 per battery 0
        assert!(r.contains("95.0%"), "{r}"); // 38000/40000 per battery 1
    }

    #[test]
    fn idle_requires_gpu_evidence() {
        let mut s = SessionData::default();
        for i in 0..3 {
            let t = i as f64;
            s.battery.push(BatteryPoint {
                t,
                discharge: Telemetry::measured(5.0, "battery", ts()),
                ..Default::default()
            });
            s.cpu.push(CpuPoint {
                t,
                utility: Telemetry::measured(2.0, "cpu", ts()),
                ..Default::default()
            });
        }
        // CPU is idle but there is no GPU sample: unknown, not idle.
        assert!(idle_subset(&s).is_empty());
        // A nearby quiet GPU sample corroborates idle.
        s.gpu.push(GpuPoint {
            t: 1.0,
            adapters: vec![GpuAdapterPoint {
                name: "igpu".to_string(),
                total_util: Telemetry::measured(0.1, "gpu", ts()),
                ..Default::default()
            }],
            ..Default::default()
        });
        assert_eq!(idle_subset(&s).len(), 3);
        // A busy GPU sample removes the idle classification again.
        s.gpu[0].adapters[0].total_util = Telemetry::measured(50.0, "gpu", ts());
        assert!(idle_subset(&s).is_empty());
    }

    #[test]
    fn single_settle_trim_applies_and_auto_keeps_stable() {
        let s = step_session(); // 40 points at t = 0..39
        // Explicit 10 s trim drops the first regime's head.
        let c = compare_single_with_settle(&s, &s, 10.0);
        assert!((c.med_a.unwrap() - 11.4).abs() < 1e-9);
        // Auto (settle == 0) on an already-stable-then-step series trims
        // nothing: first two windows agree, so settle index is 0.
        assert_eq!(
            detect_settle_end(
                &vec![5.7; 20]
                    .into_iter()
                    .chain(vec![11.4; 20])
                    .collect::<Vec<_>>(),
                1.0
            ),
            0
        );
        let auto = compare_sessions(&s, &s);
        assert!((auto.med_a.unwrap() - 8.55).abs() < 1e-9);
    }

    #[test]
    fn detect_settle_end_finds_transient() {
        // 8 s of elevated transient, then a flat 6 W regime at 1 Hz.
        let mut v = vec![12.0; 8];
        v.extend(vec![6.0; 40]);
        let idx = detect_settle_end(&v, 1.0);
        assert!(idx > 0 && idx <= 8, "idx={idx}");
        // Flat series: nothing to trim.
        assert_eq!(detect_settle_end(&vec![6.0; 48], 1.0), 0);
        // Too short: no claim.
        assert_eq!(detect_settle_end(&[1.0, 2.0], 1.0), 0);
    }

    #[test]
    fn multirep_wires_cpu_mismatch() {
        let qa = cq(60, 120.0, 50, 0.1);
        let qb = cq(60, 120.0, 50, 0.1);
        // Matched background: no mismatch confounder.
        let (_, _, cf) = experiment_confidence(&qa, &qb, Some(2.0), &[4.0, 5.0], &[4.5, 5.5]);
        assert!(!cf.iter().any(|c| c.contains("CPU mismatch")));
        // Divergent background: confounder appears (reps>1 no longer None).
        let (_, _, cf) = experiment_confidence(&qa, &qb, Some(2.0), &[4.0, 5.0], &[14.0, 15.0]);
        assert!(cf.iter().any(|c| c.contains("background CPU mismatch")));
    }

    #[test]
    fn shared_validity_gate_rejects_low_coverage() {
        let mut q = cq(60, 120.0, 50, 0.1);
        assert_eq!(
            comparison_validity(Some(6.0), Some(6.0), &q, &q, &[], Confidence::High),
            Validity::Valid
        );
        // A contaminated phase (coverage below 0.5) is Invalid even with
        // plenty of rows and no confounders.
        q.coverage = Some(0.4);
        assert_eq!(
            comparison_validity(Some(6.0), Some(6.0), &q, &q, &[], Confidence::High),
            Validity::Invalid
        );
        // Thin capture is Invalid too.
        let thin = cq(2, 1.0, 2, 0.1);
        assert_eq!(
            comparison_validity(Some(6.0), Some(6.0), &thin, &q, &[], Confidence::High),
            Validity::Invalid
        );
        // Confounders or Low confidence downgrade to Questionable, not Valid.
        let good = cq(60, 120.0, 50, 0.1);
        assert_eq!(
            comparison_validity(
                Some(6.0),
                Some(6.0),
                &good,
                &good,
                &["gap".to_string()],
                Confidence::High
            ),
            Validity::Questionable
        );
    }

    #[test]
    fn runtime_projection_shares_full_charge_basis() {
        let a = step_session();
        let mut b = step_session();
        b.label = "after".to_string();
        let med_a = Some(8.55);
        let med_b = Some(5.05);
        let (na, nb, proj) = runtime_projection(&a, &b, med_a, med_b);
        // full_mwh = 38.7 Wh in step_session; lower draw runs longer.
        assert!((na.unwrap() - 38.7 / 8.55).abs() < 1e-9);
        assert!(nb.unwrap() > na.unwrap());
        assert!((proj.unwrap() - (nb.unwrap() - na.unwrap()) * 60.0).abs() < 1e-9);
        // Without capacity/median there is no invented runtime.
        let empty = SessionData::default();
        assert_eq!(
            runtime_projection(&empty, &empty, med_a, med_b),
            (None, None, None)
        );
    }

    #[test]
    fn autocorrelated_series_loses_effective_n() {
        // 60 distinct samples but highly persistent: High is unreachable.
        let q = CaptureQuality {
            n: 60,
            duration_secs: 120.0,
            unique_values: 60,
            spread: 0.1,
            abs_spread_w: 0.1,
            max_gap_secs: 1.0,
            coverage: Some(1.0),
            rho_lag1: 0.9,
            observed_hz: None,
            median_dt_secs: 1.0,
            discontinuities: 0,
            unobserved_secs: 0.0,
        };
        let (lvl, _, cf) = ab_confidence(&q, &q, Some(2.0), None);
        // Persistence collapses the sample count: High is unreachable and
        // the discount is reported, not hidden.
        assert_eq!(lvl, Confidence::Low);
        assert!(cf.iter().any(|c| c.contains("effective n=3")));
    }

    #[test]
    fn stale_sensor_and_gap_confounders() {
        // Cached sensor: 2 distinct updates in 100 s at ~1 Hz.
        let stale = CaptureQuality {
            n: 100,
            duration_secs: 100.0,
            unique_values: 2,
            spread: 0.1,
            abs_spread_w: 0.1,
            max_gap_secs: 1.0,
            coverage: Some(1.0),
            rho_lag1: 0.0,
            observed_hz: Some(1.0),
            median_dt_secs: 1.0,
            discontinuities: 0,
            unobserved_secs: 0.0,
        };
        let (_, _, cf) = ab_confidence(&stale, &stale, Some(2.0), None);
        assert!(cf.iter().any(|c| c.contains("stale sensor")), "{cf:?}");
        // Gap > 3x cadence.
        let gappy = CaptureQuality {
            max_gap_secs: 10.0,
            median_dt_secs: 1.0,
            unique_values: 100,
            observed_hz: None,
            ..stale
        };
        let (_, _, cf) = ab_confidence(&gappy, &gappy, Some(2.0), None);
        assert!(cf.iter().any(|c| c.contains("sampling gap")), "{cf:?}");
    }

    #[test]
    fn soc_drift_confound_and_thermal_note() {
        let mut a = step_session();
        let mut b = step_session();
        b.label = "b".to_string();
        for p in &mut a.battery {
            let stamp = p.pct.stamp;
            p.pct = Telemetry::measured(80.0, p.pct.source, stamp);
        }
        for p in &mut b.battery {
            let stamp = p.pct.stamp;
            p.pct = Telemetry::measured(60.0, p.pct.source, stamp);
        }
        let c = compare_sessions(&a, &b);
        assert!(
            c.lines.iter().any(|l| l.contains("SOC drift")),
            "{:?}",
            c.lines
        );
        assert!(
            c.lines
                .iter()
                .any(|l| l.contains("thermal state unobservable"))
        );
    }

    #[test]
    fn lifecycle_across_ticks_estimates_exits() {
        // 10 ticks; pid 7 (start 7000) vanishes after tick 3.
        let mut snaps = Vec::new();
        for i in 0..10 {
            let mut top = vec![ProcEntry {
                pid: 1,
                ppid: 0,
                name: "sys.exe".to_string(),
                start_unix_ms: Some(1000),
                exit_unix_ms: None,
                cpu: Telemetry::derived(0.1, "proc", ts()),
            }];
            if i <= 3 {
                top.push(ProcEntry {
                    pid: 7,
                    ppid: 1,
                    name: "tmp.exe".to_string(),
                    start_unix_ms: Some(7000),
                    exit_unix_ms: None,
                    cpu: Telemetry::derived(5.0, "proc", ts()),
                });
            }
            if i >= 5 {
                top.push(ProcEntry {
                    pid: 9,
                    ppid: 1,
                    name: "late.exe".to_string(),
                    start_unix_ms: Some(9000),
                    exit_unix_ms: None,
                    cpu: Telemetry::derived(2.0, "proc", ts()),
                });
            }
            snaps.push(ProcPoint {
                t: i as f64,
                top,
                ..Default::default()
            });
        }
        let (starts, stops) = lifecycle_across_ticks(&snaps);
        // pid 7 absent ticks 4..9 (6 ticks > 3): estimated exit at 3+1.
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].pid, 7);
        assert!((stops[0].exit_est_t - 4.0).abs() < 1e-9);
        // pid 9 first seen at tick 5: a start.
        assert!(starts.iter().any(|s| s.pid == 9));
        // Opening-tick processes are not starts.
        assert!(!starts.iter().any(|s| s.pid == 1));
    }
}

/// Multi-timescale discharge baselines for adaptive sampling. Short
/// re-baselines automatically after a persistent regime change (rolling
/// windows); medium/long give anomaly context. Replaces the old
/// ever-growing single median, which kept flagging the rest of a session
/// after a legitimate step change.
#[derive(Debug, Default)]
pub struct MultiBaseline {
    short: std::collections::VecDeque<f64>,
    medium: std::collections::VecDeque<f64>,
    long: std::collections::VecDeque<f64>,
}

impl MultiBaseline {
    pub fn new() -> Self {
        MultiBaseline {
            short: std::collections::VecDeque::with_capacity(10),
            medium: std::collections::VecDeque::with_capacity(60),
            long: std::collections::VecDeque::with_capacity(300),
        }
    }

    pub fn push(&mut self, w: f64) {
        for (q, cap) in [
            (&mut self.short, 10),
            (&mut self.medium, 60),
            (&mut self.long, 300),
        ] {
            if q.len() >= cap {
                q.pop_front();
            }
            q.push_back(w);
        }
    }

    fn med(q: &std::collections::VecDeque<f64>) -> Option<f64> {
        stats::median(&q.iter().copied().collect::<Vec<_>>())
    }

    pub fn short_med(&self) -> Option<f64> {
        Self::med(&self.short)
    }

    pub fn medium_med(&self) -> Option<f64> {
        Self::med(&self.medium)
    }

    pub fn long_med(&self) -> Option<f64> {
        Self::med(&self.long)
    }

    /// Boost sampling when the latest value deviates from the SHORT
    /// baseline by both an absolute floor (sensor noise) and a relative
    /// fraction (scale-aware): replaces the fixed 1.5 W threshold.
    pub fn should_boost(&self, latest: f64, abs_floor_w: f64, rel_frac: f64) -> bool {
        match self.short_med() {
            Some(base) if base > 0.0 => {
                (latest - base).abs() >= abs_floor_w && (latest - base).abs() >= rel_frac * base
            }
            _ => false,
        }
    }
}

// ------------------------- helpers moved from collectors -------------------------
// These lived in pf-collectors (battery/cpu/gpu/proc) but the analysis
// engine needs them, and collectors must not depend on pf-core's analysis
// callers. Canonical home is here; the collectors re-export them.

/// Per-physical-battery health: full-charge over design from the SAME
/// interface. None when either capacity is missing or invalid — never pair
/// an aggregate Windows full-charge figure with one battery's design.
pub fn per_battery_health(full_mwh: Option<f64>, designed_mwh: Option<f64>) -> Option<f64> {
    match (full_mwh, designed_mwh) {
        (Some(f), Some(d)) => crate::stats::health_ratio(f, d),
        _ => None,
    }
}

/// Headline health verdict with same-basis pairing enforced.
#[derive(Debug, Clone)]
pub struct HeadlineHealth {
    /// Headline ratio. None on multi-battery systems (aggregates must not
    /// be paired) or when the single-battery basis is incomplete.
    pub value: Option<f64>,
    /// Why the headline is unavailable, if it is.
    pub reason: Option<String>,
    /// Per-physical-battery (index, health) in interface order.
    pub per_battery: Vec<(u32, Option<f64>)>,
}

/// Headline health for a session's battery metadata. Multi-battery systems
/// (`battery_count > 1` or several queried interfaces) get
/// `value = None` with reason "multi-battery: see per-battery" — pairing
/// the aggregate full-charge with the primary design would be a
/// cross-basis fabrication. Single-battery systems keep full/design,
/// preferring the single queried interface when exactly one exists.
pub fn headline_health(meta: &crate::session::BatteryMeta) -> HeadlineHealth {
    let per_battery: Vec<(u32, Option<f64>)> = meta
        .batteries
        .iter()
        .map(|b| (b.index, per_battery_health(b.full_mwh, b.designed_mwh)))
        .collect();
    let multi = meta.battery_count.is_some_and(|c| c > 1) || meta.batteries.len() > 1;
    if multi {
        return HeadlineHealth {
            value: None,
            reason: Some("multi-battery: see per-battery".to_string()),
            per_battery,
        };
    }
    let value = if per_battery.len() == 1 {
        per_battery[0].1
    } else {
        per_battery_health(meta.full_mwh, meta.design_mwh)
    };
    HeadlineHealth {
        value,
        reason: value
            .is_none()
            .then_some("design capacity unknown".to_string()),
        per_battery,
    }
}

/// Background CPU mismatch between two conditions in percentage points.
///
/// Takes per-block CPU-utility medians for condition A and B (see
/// `analysis::block_cpu_median`), compares the median-of-medians, and
/// returns the absolute gap. None when either side has no usable blocks —
/// unknown background, not zero background. Used by the multi-rep
/// experiment path so reps>1 checks background mismatch instead of
/// passing None.
pub fn aggregate_cpu_mismatch(a_cpu_meds: &[f64], b_cpu_meds: &[f64]) -> Option<f64> {
    match (
        crate::stats::median(a_cpu_meds),
        crate::stats::median(b_cpu_meds),
    ) {
        (Some(x), Some(y)) => Some((y - x).abs()),
        _ => None,
    }
}

/// Engine-% sum that counts as "active" for awake detection.
pub const ACTIVE_UTIL_PCT: f64 = 0.5;

/// A vendor GPU-power source. Modularity point: each entry describes one
/// backend (units, cadence, privilege needs, limits) and whether it is
/// actually available on this machine. Unavailable backends are reported
/// with reasons — watts are never synthesized.
#[derive(Debug, Clone)]
pub struct PowerAdapter {
    pub name: String,
    pub available: bool,
    pub provenance: String,
    pub units: String,
    pub cadence_ms: Option<u64>,
    pub requires_admin: bool,
    pub limitations: String,
}

fn unavailable(
    name: &str,
    provenance: &str,
    requires_admin: bool,
    limitations: &str,
) -> PowerAdapter {
    PowerAdapter {
        name: name.to_string(),
        available: false,
        provenance: provenance.to_string(),
        units: "W".to_string(),
        cadence_ms: None,
        requires_admin,
        limitations: limitations.to_string(),
    }
}

/// Power backends known to this build and their availability here.
/// Currently every backend is unavailable (no vendor power library is
/// linked): the entries carry the reason, not fake watts. Callers must
/// surface the unavailability instead of inventing GPU power.
pub fn available_adapters() -> Vec<PowerAdapter> {
    vec![
        unavailable(
            "NVML (NVIDIA)",
            "unavailable",
            false,
            "NVML client not linked on this build; no NVIDIA power readings",
        ),
        unavailable(
            "ADL (AMD)",
            "unavailable",
            false,
            "ADL client not linked on this build; no AMD dGPU power readings",
        ),
        unavailable(
            "Intel (IGPU/dGPU)",
            "unavailable",
            false,
            "no generic Intel GPU power API on Windows; iGPU power not separately measurable",
        ),
    ]
}

/// Estimated exit tick from absence across consecutive snapshots.
///
/// True process exit needs kernel tracing (ETW, privileged). Without it,
/// the honest heuristic is: a (pid, start-time) identity seen at
/// `last_seen_t`, then absent for `missed_ticks` consecutive ticks of
/// `interval` spacing, is estimated to have exited at
/// `last_seen_t + interval`. Returns None unless `missed_ticks > 3` —
/// shorter absences are dropout/top-N churn, not exits. The caller must
/// label the result estimated. Units follow the caller (seconds or ms).
pub fn exit_hint(last_seen_t: f64, interval: f64, missed_ticks: usize) -> Option<f64> {
    if missed_ticks > 3 && interval > 0.0 && last_seen_t.is_finite() {
        Some(last_seen_t + interval)
    } else {
        None
    }
}

#[cfg(test)]
mod baseline_tests {
    use super::*;

    #[test]
    fn short_rebaselines_after_regime_change() {
        let mut b = MultiBaseline::new();
        for _ in 0..10 {
            b.push(5.0);
        }
        assert!(!b.should_boost(5.2, 1.0, 0.2));
        assert!(b.should_boost(11.0, 1.0, 0.2));
        // Persistent new regime: short window forgets the old one.
        for _ in 0..10 {
            b.push(11.0);
        }
        assert!(!b.should_boost(11.0, 1.0, 0.2));
        assert_eq!(b.short_med(), Some(11.0));
        // Medium still remembers the transition (context, not trigger).
        assert!(b.medium_med().unwrap() < 11.0);
    }

    #[test]
    fn relative_floor_scales() {
        let mut b = MultiBaseline::new();
        for _ in 0..10 {
            b.push(50.0);
        }
        // +5 W is above the 1 W floor but below 20% of 50 W: no boost.
        assert!(!b.should_boost(55.0, 1.0, 0.2));
        assert!(b.should_boost(65.0, 1.0, 0.2));
        // Tiny baseline: absolute floor dominates, not the fraction.
        let mut c = MultiBaseline::new();
        for _ in 0..10 {
            c.push(2.0);
        }
        assert!(!c.should_boost(2.3, 1.0, 0.2));
        assert!(c.should_boost(3.5, 1.0, 0.2));
    }
}
