//! Pure, testable math: percentiles, energy integration, runtime estimates.
//!
//! No hardware access here. All functions are deterministic and unit-tested.

/// Median of a non-empty slice. Returns None for empty input (never fake it).
pub fn median(values: &[f64]) -> Option<f64> {
    percentile_sorted(&mut values.to_vec(), 50.0)
}

/// Percentile with linear interpolation. `p` in 0..=100.
pub fn percentile_sorted(values: &mut [f64], p: f64) -> Option<f64> {
    if values.is_empty() || !(0.0..=100.0).contains(&p) {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let rank = p / 100.0 * (values.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        Some(values[lo])
    } else {
        let frac = rank - lo as f64;
        Some(values[lo] * (1.0 - frac) + values[hi] * frac)
    }
}

pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

/// Energy integrated over contiguous known segments, plus how much of the
/// observed timeline was actually covered. A pair of samples is integrated
/// only when both are known AND separated by at most `max_gap_secs`;
/// everything else is counted as unknown instead of being bridged.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SegmentEnergy {
    pub energy_wh: f64,
    pub covered_secs: f64,
    pub unknown_secs: f64,
    /// Real elapsed time refused because a clock discontinuity forced a
    /// segment boundary (never integrated). Reported separately so callers
    /// can say what interval was unobserved, not merely that coverage fell.
    pub unobserved_secs: f64,
    /// Number of forced segment boundaries (clock discontinuities).
    pub discontinuities: usize,
}

impl SegmentEnergy {
    /// Covered fraction of the observed span in `0..=1`; None when the span
    /// is zero (no meaningful coverage claim).
    pub fn coverage(&self) -> Option<f64> {
        let span = self.covered_secs + self.unknown_secs;
        if span <= 0.0 {
            None
        } else {
            Some(self.covered_secs / span)
        }
    }
}

/// Why a segment boundary was forced between two consecutive samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscontinuityKind {
    /// One clock (or both) advanced further than `max_gap_secs`: samples
    /// are missing.
    SamplingGap,
    /// Wall advanced much more than monotonic: suspend/resume, or a forward
    /// wall-clock step (NTP/hibernate) while the monotonic clock slept.
    WallAheadOfMono,
    /// Monotonic advanced much more than wall: a monotonic-clock jump while
    /// wall stayed put (cannot be trusted as elapsed real time).
    MonoAheadOfWall,
    /// A clock moved backwards.
    ClockWentBackwards,
}

impl DiscontinuityKind {
    /// Stable wire/display name (also the footer summary key suffix).
    pub fn as_str(self) -> &'static str {
        match self {
            DiscontinuityKind::SamplingGap => "sampling_gap",
            DiscontinuityKind::WallAheadOfMono => "wall_ahead_of_mono",
            DiscontinuityKind::MonoAheadOfWall => "mono_ahead_of_wall",
            DiscontinuityKind::ClockWentBackwards => "clock_went_backwards",
        }
    }

    /// Index into a per-kind count array, stable across versions.
    pub fn index(self) -> usize {
        match self {
            DiscontinuityKind::SamplingGap => 0,
            DiscontinuityKind::WallAheadOfMono => 1,
            DiscontinuityKind::MonoAheadOfWall => 2,
            DiscontinuityKind::ClockWentBackwards => 3,
        }
    }

    /// All kinds, in index order.
    pub const ALL: [DiscontinuityKind; 4] = [
        DiscontinuityKind::SamplingGap,
        DiscontinuityKind::WallAheadOfMono,
        DiscontinuityKind::MonoAheadOfWall,
        DiscontinuityKind::ClockWentBackwards,
    ];
}

/// One forced boundary between samples `index` and `index + 1`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Discontinuity {
    pub index: usize,
    pub kind: DiscontinuityKind,
    pub mono_gap_secs: f64,
    /// Wall gap, or None when wall time was unavailable (stamp 0).
    pub wall_gap_secs: Option<f64>,
    /// Real elapsed time we must not integrate across.
    pub unobserved_secs: f64,
}

/// Classify the step between two consecutive samples carrying both clocks.
/// `None` means the pair is continuous and safe to integrate; `Some(kind)`
/// forces a segment boundary. This is the single source of truth shared by
/// the batch scan ([`discontinuities`]) and the streaming live accumulator
/// (`pf-agent`), so live and recovered evidence apply identical rules.
pub fn classify_step(
    mono_dt: f64,
    wall_dt: Option<f64>,
    max_gap_secs: f64,
    max_skew_secs: f64,
) -> Option<DiscontinuityKind> {
    if mono_dt < 0.0 || wall_dt.is_some_and(|w| w < 0.0) {
        Some(DiscontinuityKind::ClockWentBackwards)
    } else if let Some(w) = wall_dt {
        if w - mono_dt > max_skew_secs {
            Some(DiscontinuityKind::WallAheadOfMono)
        } else if mono_dt - w > max_skew_secs {
            Some(DiscontinuityKind::MonoAheadOfWall)
        } else if mono_dt > max_gap_secs || w > max_gap_secs {
            Some(DiscontinuityKind::SamplingGap)
        } else {
            None
        }
    } else if mono_dt > max_gap_secs {
        Some(DiscontinuityKind::SamplingGap)
    } else {
        None
    }
}

/// Default segment boundary for energy integration: three collection
/// intervals, floored at 5 s. Shared by the live footer and recovery so a
/// clean and a recovered session apply the same gap rule.
pub fn energy_max_gap_secs(interval_ms: u64) -> f64 {
    (interval_ms as f64 / 1000.0 * 3.0).max(5.0)
}

/// Deterministic discontinuity scan over a series carrying BOTH clocks.
///
/// A boundary is forced when either clock shows a gap larger than
/// `max_gap_secs`, when the two clocks diverge by more than `max_skew_secs`,
/// or when a clock steps backwards. Platform clocks differ in whether they
/// advance during sleep, so a divergence in either direction is treated as
/// real (suspend/resume or clock adjustment) — we never assume mono covers
/// wall or vice versa. `wall_secs` may be shorter than `times_secs` or hold
/// zeros (archive v1 / fixtures); wall is then treated as unavailable and
/// only the monotonic checks apply.
pub fn discontinuities(
    times_secs: &[f64],
    wall_secs: &[f64],
    max_gap_secs: f64,
    max_skew_secs: f64,
) -> Vec<Discontinuity> {
    let n = times_secs.len();
    if n < 2 {
        return Vec::new();
    }
    let wall_aligned = wall_secs.len() == n;
    let mut out = Vec::new();
    for i in 0..n - 1 {
        let mono_dt = times_secs[i + 1] - times_secs[i];
        // Wall is only meaningful when both endpoints carry a real stamp.
        let wall_dt = if wall_aligned && wall_secs[i] > 0.0 && wall_secs[i + 1] > 0.0 {
            Some(wall_secs[i + 1] - wall_secs[i])
        } else {
            None
        };
        // Divergence is checked before the raw gap so a wall-clock jump is
        // reported as divergence (the more diagnostic cause) even when the
        // wall gap also exceeds max_gap_secs. If both clocks gap together,
        // the divergence test passes and it is classified as a sampling gap.
        let Some(kind) = classify_step(mono_dt, wall_dt, max_gap_secs, max_skew_secs) else {
            continue;
        };
        let unobserved = wall_dt.map(|w| mono_dt.max(w)).unwrap_or(mono_dt).max(0.0);
        out.push(Discontinuity {
            index: i,
            kind,
            mono_gap_secs: mono_dt,
            wall_gap_secs: wall_dt,
            unobserved_secs: unobserved,
        });
    }
    out
}

/// Shared trapezoid loop: a pair is integrated only when it is not separated
/// by a forced boundary, both endpoints are known, and the monotonic step is
/// within `max_gap_secs`. `breaks[i]` forces a boundary before sample i+1.
fn integrate_pairs(
    times_secs: &[f64],
    watts: &[Option<f64>],
    max_gap_secs: f64,
    breaks: &[bool],
) -> SegmentEnergy {
    let mut out = SegmentEnergy::default();
    if times_secs.len() != watts.len() || times_secs.len() < 2 {
        return out;
    }
    for i in 0..times_secs.len() - 1 {
        let dt = times_secs[i + 1] - times_secs[i];
        if dt < 0.0 || breaks.get(i).copied().unwrap_or(false) {
            if dt > 0.0 {
                out.unknown_secs += dt;
            }
            continue; // backwards clock / discontinuity: never invent energy
        }
        match (watts[i], watts[i + 1]) {
            (Some(a), Some(b)) if dt <= max_gap_secs => {
                out.energy_wh += 0.5 * (a + b) * dt / 3600.0;
                out.covered_secs += dt;
            }
            _ => out.unknown_secs += dt,
        }
    }
    out
}

/// Segment-aware trapezoidal integration. `watts[i]` is `None` for any
/// sample that is not in the integrated state (charging, idle, unavailable),
/// so a charging or missing interval can never contribute phantom energy.
pub fn integrate_segmented(
    times_secs: &[f64],
    watts: &[Option<f64>],
    max_gap_secs: f64,
) -> SegmentEnergy {
    integrate_pairs(times_secs, watts, max_gap_secs, &[])
}

/// Both-clock integration: forces a boundary at every [`discontinuities`]
/// hit (gap in EITHER clock, or wall/mono divergence) before delegating to
/// the same trapezoid loop as [`integrate_segmented`]. The returned
/// `unobserved_secs`/`discontinuities` report what was skipped.
pub fn integrate_segmented_clocks(
    times_secs: &[f64],
    wall_secs: &[f64],
    watts: &[Option<f64>],
    max_gap_secs: f64,
    max_skew_secs: f64,
) -> SegmentEnergy {
    let disc = discontinuities(times_secs, wall_secs, max_gap_secs, max_skew_secs);
    let mut breaks = vec![false; times_secs.len().saturating_sub(1)];
    let mut unobserved_secs = 0.0;
    for d in &disc {
        if d.index < breaks.len() {
            breaks[d.index] = true;
        }
        unobserved_secs += d.unobserved_secs;
    }
    let mut out = integrate_pairs(times_secs, watts, max_gap_secs, &breaks);
    out.unobserved_secs = unobserved_secs;
    out.discontinuities = disc.len();
    out
}

/// Estimated runtime in hours from remaining energy and average discharge.
/// Returns None for non-positive discharge (unknown, not infinite).
pub fn runtime_hours(remaining_wh: f64, avg_discharge_w: f64) -> Option<f64> {
    if remaining_wh < 0.0 || avg_discharge_w <= 0.0 {
        return None;
    }
    Some(remaining_wh / avg_discharge_w)
}

/// Full-charge normalized runtime in hours: full capacity over median draw.
///
/// Unlike `runtime_hours` (each session's own remaining energy), this puts
/// two sessions on the same energy basis so different starting charges do
/// not masquerade as efficiency differences. None for invalid inputs.
pub fn normalized_runtime_h(median_w: f64, full_wh: f64) -> Option<f64> {
    if !median_w.is_finite() || !full_wh.is_finite() || median_w <= 0.0 || full_wh < 0.0 {
        return None;
    }
    Some(full_wh / median_w)
}

/// Lag-1 autocorrelation of a demeaned series in [-1, 1]. Returns 0.0 for
/// < 3 samples or zero variance (no measurable persistence). Deterministic,
/// no dependencies.
pub fn lag1_autocorr(vals: &[f64]) -> f64 {
    let n = vals.len();
    if n < 3 {
        return 0.0;
    }
    let mean = vals.iter().sum::<f64>() / n as f64;
    let mut denom = 0.0;
    let mut numer = 0.0;
    for v in vals {
        denom += (v - mean).powi(2);
    }
    if denom.is_nan() || denom <= 0.0 {
        return 0.0;
    }
    for i in 0..n - 1 {
        numer += (vals[i] - mean) * (vals[i + 1] - mean);
    }
    (numer / denom).clamp(-1.0, 1.0)
}

/// Effective sample size under AR(1)-style persistence:
/// `eff = n * (1 - rho) / (1 + rho)`, capped by the distinct-value count
/// (a cached sensor contributes no independent samples) and clamped to
/// [1, n]. The lower clamp is 1, not 2, so the distinct-value cap is never
/// overridden: one repeated reading stays one independent sample.
/// Returns n for n < 2.
pub fn effective_n(n: usize, unique: usize, rho: f64) -> usize {
    if n < 2 {
        return n;
    }
    let rho = rho.clamp(-0.9, 0.99);
    let raw = n as f64 * (1.0 - rho) / (1.0 + rho);
    let eff = raw.min(unique as f64).min(n as f64);
    eff.clamp(1.0, n as f64).round() as usize
}

/// Battery health ratio full_charge / design. None when inputs invalid.
pub fn health_ratio(full_charge: f64, design: f64) -> Option<f64> {
    if design <= 0.0 || full_charge < 0.0 {
        return None;
    }
    Some(full_charge / design)
}

/// Simple moving average over the last `window` points.
pub fn rolling_mean(values: &[f64], window: usize) -> Option<f64> {
    if values.is_empty() || window == 0 {
        return None;
    }
    let n = values.len().min(window);
    mean(&values[values.len() - n..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_odd_even_empty() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn percentiles_match_spec_example() {
        let mut v = vec![5.9, 5.1, 5.4, 5.8, 5.6];
        assert_eq!(percentile_sorted(&mut v.clone(), 50.0), Some(5.6));
        assert_eq!(percentile_sorted(&mut v.clone(), 10.0), Some(5.22));
        assert_eq!(percentile_sorted(&mut v.clone(), 90.0), Some(5.86));
        assert_eq!(percentile_sorted(&mut v, 50.0), Some(5.6));
        assert_eq!(percentile_sorted(&mut [], 50.0), None);
        assert_eq!(percentile_sorted(&mut [1.0], 101.0), None);
    }

    #[test]
    fn energy_integration_constant_load() {
        // 6 W for 1 hour = 6 Wh.
        let t = vec![0.0, 1800.0, 3600.0];
        let w = vec![Some(6.0), Some(6.0), Some(6.0)];
        let seg = integrate_segmented(&t, &w, f64::INFINITY);
        assert!((seg.energy_wh - 6.0).abs() < 1e-9);
        assert!((seg.coverage().unwrap() - 1.0).abs() < 1e-9);
        // A backwards clock never invents energy.
        let back = integrate_segmented(&[5.0, 1.0], &[Some(6.0), Some(6.0)], f64::INFINITY);
        assert_eq!(back.energy_wh, 0.0);
    }

    #[test]
    fn energy_integration_rejects_bad_input() {
        assert_eq!(
            integrate_segmented(&[0.0], &[Some(6.0)], 5.0).energy_wh,
            0.0
        );
        assert_eq!(
            integrate_segmented(&[0.0, 1.0], &[Some(6.0)], 5.0).energy_wh,
            0.0
        );
    }

    #[test]
    fn segmented_integration_excludes_gaps_and_unknown_states() {
        // 6 W over two 1 s windows with a 98 s gap between them.
        let t = vec![0.0, 1.0, 2.0, 100.0, 101.0];
        let w = vec![Some(6.0), Some(6.0), Some(6.0), Some(6.0), Some(6.0)];
        let seg = integrate_segmented(&t, &w, 5.0);
        assert!((seg.energy_wh - 6.0 * 3.0 / 3600.0).abs() < 1e-12);
        assert!((seg.covered_secs - 3.0).abs() < 1e-9);
        assert!((seg.unknown_secs - 98.0).abs() < 1e-9);
        assert!((seg.coverage().unwrap() - 3.0 / 101.0).abs() < 1e-9);

        // A charging/unknown sample in the middle is not bridged to watts.
        let w2 = vec![Some(6.0), None, Some(6.0)];
        let seg2 = integrate_segmented(&[0.0, 1.0, 2.0], &w2, 5.0);
        assert_eq!(seg2.energy_wh, 0.0);
        assert_eq!(seg2.covered_secs, 0.0);
        assert!((seg2.unknown_secs - 2.0).abs() < 1e-9);

        // No span => no coverage claim.
        assert_eq!(SegmentEnergy::default().coverage(), None);
    }

    #[test]
    fn mono_gap_forces_boundary_without_phantom_energy() {
        // 6 W over two 1 s windows, then a 300 s monotonic gap.
        let t = vec![0.0, 1.0, 2.0, 302.0];
        let wall = vec![1000.0, 1001.0, 1002.0, 1302.0];
        let w = vec![Some(6.0), Some(6.0), Some(6.0), Some(6.0)];
        let seg = integrate_segmented_clocks(&t, &wall, &w, 5.0, 2.0);
        // Only the two 1 s pairs on each side are integrated; the 300 s gap
        // contributes no energy and is reported as unobserved.
        assert!((seg.energy_wh - 6.0 * 2.0 / 3600.0).abs() < 1e-12);
        assert!((seg.covered_secs - 2.0).abs() < 1e-9);
        assert_eq!(seg.discontinuities, 1);
        assert!((seg.unobserved_secs - 300.0).abs() < 1e-9);
        assert_eq!(
            discontinuities(&t, &wall, 5.0, 2.0)[0].kind,
            DiscontinuityKind::SamplingGap
        );
    }

    #[test]
    fn wall_jump_forward_while_mono_continuous_forces_boundary() {
        // Mono ticks 1 s, wall jumps 600 s (NTP step / resume with a sleeping
        // monotonic clock). The mono dt alone would look continuous.
        let t = vec![0.0, 1.0, 2.0, 3.0];
        let wall = vec![1000.0, 1001.0, 1601.0, 1602.0];
        let w = vec![Some(6.0), Some(6.0), Some(6.0), Some(6.0)];
        let d = discontinuities(&t, &wall, 5.0, 2.0);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DiscontinuityKind::WallAheadOfMono);
        let seg = integrate_segmented_clocks(&t, &wall, &w, 5.0, 2.0);
        assert!(seg.energy_wh < 3.0 * 6.0 / 3600.0);
        assert!((seg.unobserved_secs - 600.0).abs() < 1e-9);
    }

    #[test]
    fn mono_jump_while_wall_continuous_forces_boundary() {
        // Wall stays continuous (1 s cadence); mono jumps 600 s, so mono
        // cannot be trusted as elapsed time.
        let t = vec![0.0, 1.0, 602.0, 603.0];
        let wall = vec![1000.0, 1001.0, 1002.0, 1003.0];
        let w = vec![Some(6.0), Some(6.0), Some(6.0), Some(6.0)];
        let d = discontinuities(&t, &wall, 5.0, 2.0);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DiscontinuityKind::MonoAheadOfWall);
        let seg = integrate_segmented_clocks(&t, &wall, &w, 5.0, 2.0);
        assert_eq!(seg.discontinuities, 1);
    }

    #[test]
    fn continuous_series_is_single_segment_unchanged() {
        // Both clocks advance together: no boundary, identical to the
        // single-clock result.
        let t = vec![0.0, 1.0, 2.0, 3.0, 4.0];
        let wall = vec![1000.0, 1001.0, 1002.0, 1003.0, 1004.0];
        let w = vec![Some(6.0), Some(5.0), Some(7.0), Some(6.0), Some(6.0)];
        let both = integrate_segmented_clocks(&t, &wall, &w, 5.0, 2.0);
        let mono = integrate_segmented(&t, &w, 5.0);
        assert_eq!(both, mono);
        assert_eq!(both.discontinuities, 0);
        assert_eq!(both.unobserved_secs, 0.0);
        assert!(discontinuities(&t, &wall, 5.0, 2.0).is_empty());
    }

    #[test]
    fn wall_unavailable_falls_back_to_mono_only() {
        // Archive v1 / fixtures stamp wall 0; those must not be read as
        // divergence and must behave exactly like the mono-only path.
        let t = vec![0.0, 1.0, 2.0];
        let wall = vec![0.0, 0.0, 0.0];
        let w = vec![Some(6.0), Some(6.0), Some(6.0)];
        assert_eq!(
            integrate_segmented_clocks(&t, &wall, &w, 5.0, 1.0),
            integrate_segmented(&t, &w, 5.0)
        );
        assert!(discontinuities(&t, &wall, 5.0, 1.0).is_empty());
    }

    #[test]
    fn backwards_clock_forces_boundary() {
        let t = vec![0.0, 5.0, 1.0];
        let wall = vec![1000.0, 1005.0, 1001.0];
        let w = vec![Some(6.0), Some(6.0), Some(6.0)];
        let d = discontinuities(&t, &wall, 10.0, 2.0);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DiscontinuityKind::ClockWentBackwards);
        // Only the first (forward, continuous) pair is integrated; the
        // backwards step is a boundary, never negative/phantom energy.
        let seg = integrate_segmented_clocks(&t, &wall, &w, 10.0, 2.0);
        assert!((seg.energy_wh - 6.0 * 5.0 / 3600.0).abs() < 1e-12);
        assert_eq!(seg.discontinuities, 1);
    }

    #[test]
    fn runtime_needs_positive_discharge() {
        assert_eq!(runtime_hours(38.7, 5.8), Some(38.7 / 5.8));
        assert_eq!(runtime_hours(38.7, 0.0), None);
        assert_eq!(runtime_hours(38.7, -2.0), None);
    }

    #[test]
    fn normalized_runtime_same_basis() {
        // Same full charge, different draws: slower draw projects longer.
        assert_eq!(normalized_runtime_h(5.0, 40.0), Some(8.0));
        assert_eq!(normalized_runtime_h(10.0, 40.0), Some(4.0));
        assert_eq!(normalized_runtime_h(0.0, 40.0), None);
        assert_eq!(normalized_runtime_h(-1.0, 40.0), None);
        assert_eq!(normalized_runtime_h(5.0, -1.0), None);
        assert_eq!(normalized_runtime_h(f64::NAN, 40.0), None);
    }

    #[test]
    fn lag1_autocorr_behaves() {
        assert_eq!(lag1_autocorr(&[]), 0.0);
        assert_eq!(lag1_autocorr(&[1.0, 2.0]), 0.0);
        // Constant series: no variance, no measurable persistence.
        assert_eq!(lag1_autocorr(&[5.0, 5.0, 5.0, 5.0]), 0.0);
        // Alternating series is strongly anti-correlated.
        assert!(lag1_autocorr(&[0.0, 10.0, 0.0, 10.0, 0.0, 10.0]) < -0.5);
        // Persistent two-regime series is positively correlated.
        let mut step = vec![5.0; 20];
        step.extend(vec![11.0; 20]);
        assert!(lag1_autocorr(&step) > 0.5);
    }

    #[test]
    fn effective_n_discounts_persistence() {
        // Independent samples: full credit (capped by unique).
        assert_eq!(effective_n(60, 60, 0.0), 60);
        assert_eq!(effective_n(60, 10, 0.0), 10);
        // Strong persistence collapses to the floor.
        assert_eq!(effective_n(60, 60, 0.9), 3);
        // Anti-correlation never invents samples beyond n.
        assert_eq!(effective_n(60, 60, -0.9), 60);
        // Tiny inputs pass through.
        assert_eq!(effective_n(1, 1, 0.9), 1);
        assert_eq!(effective_n(0, 0, 0.0), 0);
    }

    #[test]
    fn effective_n_respects_distinct_value_cap() {
        // One repeated reading is one independent sample, not the floor 2.
        assert_eq!(effective_n(60, 1, 0.0), 1);
        // A cached sensor with few updates cannot exceed its distinct count.
        assert_eq!(effective_n(100, 2, 0.0), 2);
        assert_eq!(effective_n(60, 3, 0.5), 3);
        // Normal varying data is discounted by persistence, not by the cap.
        assert_eq!(effective_n(60, 60, 0.5), 20);
    }

    #[test]
    fn health_ratio_guards_division() {
        assert_eq!(health_ratio(38.7, 42.0), Some(38.7 / 42.0));
        assert_eq!(health_ratio(38.7, 0.0), None);
    }

    #[test]
    fn rolling_mean_uses_tail_window() {
        assert_eq!(rolling_mean(&[1.0, 2.0, 3.0, 4.0], 2), Some(3.5));
        assert_eq!(rolling_mean(&[1.0], 5), Some(1.0));
        assert_eq!(rolling_mean(&[], 5), None);
    }
}
