//! Deterministic, observational comparison of two session ranges.
//!
//! This layer **consumes** the existing range analysis outputs — quality,
//! energy integration, domain statistics, process evidence and correlation —
//! and adds only comparison-specific evidence: per-metric deltas with honest
//! evidence, per-metric comparability (never one global binary flag),
//! distribution and reliability summaries, categorical context differences and
//! explicit caveats.
//!
//! It never attributes cause, never downgrades an absent reading to zero and
//! never produces a percentage change against a zero or unsuitable
//! denominator. A whole session is simply a range covering its full evidence
//! span, so session-vs-session and range-vs-range share one implementation.

use std::collections::BTreeMap;

use crate::analyze::{
    self, Correlation, DomainStat, MetricSeries, MetricStat, ProcessAnalysis, RangeEnergy,
    RangeQuality, Sample, Stat,
};
use crate::session::SessionData;
use crate::stats;

// ---------------------------------------------------------------------------
// Input abstraction
// ---------------------------------------------------------------------------

/// One side of a comparison: a session plus an optional wall-clock range.
///
/// `from_ms`/`to_ms` are `None` for "whole session", which resolves to the
/// session's complete evidence span. Both sides are expressed with this one
/// type, so session-vs-session and range-vs-range never fork the analysis.
#[derive(Clone, Copy)]
pub struct ComparisonInput<'a> {
    pub session: &'a SessionData,
    pub interval_ms: u64,
    pub from_ms: Option<u64>,
    pub to_ms: Option<u64>,
}

impl<'a> ComparisonInput<'a> {
    /// A side covering the whole recorded evidence span.
    pub fn whole(session: &'a SessionData, interval_ms: u64) -> Self {
        ComparisonInput {
            session,
            interval_ms,
            from_ms: None,
            to_ms: None,
        }
    }

    /// A side covering an explicitly selected interval.
    pub fn range(session: &'a SessionData, interval_ms: u64, from_ms: u64, to_ms: u64) -> Self {
        ComparisonInput {
            session,
            interval_ms,
            from_ms: Some(from_ms),
            to_ms: Some(to_ms),
        }
    }

    /// Resolve the effective interval. A whole-session input uses the session
    /// span; one-sided inputs extend to the span edge. Bounds are normalized.
    pub fn resolved_range(&self) -> (u64, u64) {
        let (start, end) = analyze::session_span_ms(self.session);
        let from = self.from_ms.unwrap_or(start);
        let to = self.to_ms.unwrap_or(end);
        (from.min(to), from.max(to))
    }
}

// ---------------------------------------------------------------------------
// Comparability
// ---------------------------------------------------------------------------

/// How meaningful a comparison is. Deliberately not a single boolean: each
/// metric can land on a different level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparabilityLevel {
    Compatible,
    CompatibleWithCaveats,
    Weak,
    NotComparable,
}

impl ComparabilityLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ComparabilityLevel::Compatible => "compatible",
            ComparabilityLevel::CompatibleWithCaveats => "compatible_with_caveats",
            ComparabilityLevel::Weak => "weak",
            ComparabilityLevel::NotComparable => "not_comparable",
        }
    }

    fn rank(self) -> u8 {
        match self {
            ComparabilityLevel::Compatible => 0,
            ComparabilityLevel::CompatibleWithCaveats => 1,
            ComparabilityLevel::Weak => 2,
            ComparabilityLevel::NotComparable => 3,
        }
    }

    fn max(self, other: ComparabilityLevel) -> ComparabilityLevel {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }
}

/// One comparability finding. `metric` is `None` for a comparison-wide
/// finding, or the metric key it applies to.
#[derive(Debug, Clone, PartialEq)]
pub struct ComparabilityFinding {
    pub metric: Option<&'static str>,
    pub level: ComparabilityLevel,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Comparability {
    pub overall: ComparabilityLevel,
    pub findings: Vec<ComparabilityFinding>,
}

// ---------------------------------------------------------------------------
// Reliability & distributions
// ---------------------------------------------------------------------------

/// Conservative reliability of the observed difference. Statistical
/// significance is never faked: the effective sample count discounts
/// autocorrelated telemetry, and a difference below the measured spread is
/// reported as within noise rather than as a real effect.
#[derive(Debug, Clone, PartialEq)]
pub struct Reliability {
    /// "distinguishable" | "weak_evidence" | "within_noise" | "insufficient"
    pub level: &'static str,
    pub note: String,
    pub noise_floor: Option<f64>,
    /// Standardized effect: |delta| / pooled robust spread. `None` when the
    /// spread is zero or unknown.
    pub effect_size: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DistributionSummary {
    pub n: usize,
    pub known: usize,
    pub min: Option<f64>,
    pub p10: Option<f64>,
    pub median: Option<f64>,
    pub p90: Option<f64>,
    pub max: Option<f64>,
    /// Robust spread (p90 - p10); `None` without both percentiles.
    pub spread: Option<f64>,
}

impl DistributionSummary {
    fn of(m: Option<&MetricStat>) -> DistributionSummary {
        match m {
            Some(m) if m.stat.known > 0 => DistributionSummary {
                n: m.stat.n,
                known: m.stat.known,
                min: m.stat.min,
                p10: m.stat.p10,
                median: m.stat.median,
                p90: m.stat.p90,
                max: m.stat.max,
                spread: spread_of(&m.stat),
            },
            other => DistributionSummary {
                n: other.map(|m| m.stat.n).unwrap_or(0),
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DistributionComparison {
    pub metric: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub a: DistributionSummary,
    pub b: DistributionSummary,
}

// ---------------------------------------------------------------------------
// Metric comparison
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct MetricComparison {
    pub metric: &'static str,
    pub label: &'static str,
    pub domain: &'static str,
    pub unit: &'static str,
    pub a: Option<f64>,
    pub b: Option<f64>,
    pub absolute_delta: Option<f64>,
    /// Only present when the denominator is non-zero and the unit supports a
    /// meaningful relative change; never emitted for a zero/near-zero base.
    pub relative_delta: Option<f64>,
    /// "increased" | "decreased" | "unchanged" | "unavailable" | "missing_b" | "missing_a"
    pub direction: &'static str,
    pub sample_count_a: usize,
    pub sample_count_b: usize,
    pub coverage_a: Option<f64>,
    pub coverage_b: Option<f64>,
    pub provenance_a: &'static str,
    pub provenance_b: &'static str,
    pub comparability: ComparabilityLevel,
    pub comparability_reason: String,
    /// Human evidence line, e.g. "measured on both sides" / "estimated on B".
    pub evidence: String,
    pub reliability: Reliability,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedDifference {
    pub metric: &'static str,
    pub label: &'static str,
    pub domain: &'static str,
    pub unit: &'static str,
    pub delta: f64,
    pub relative_delta: Option<f64>,
    pub direction: &'static str,
    /// Deterministic relevance score (|delta| relative to the noise floor).
    pub relevance: f64,
    pub comparability: ComparabilityLevel,
    pub reliability: &'static str,
    pub basis: String,
}

// ---------------------------------------------------------------------------
// Domains, processes, categorical, correlations, caveats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct DomainMetric {
    pub metric: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub a_median: Option<f64>,
    pub b_median: Option<f64>,
    pub delta: Option<f64>,
    pub known_a: usize,
    pub known_b: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DomainComparison {
    pub domain: &'static str,
    pub available_a: bool,
    pub available_b: bool,
    /// Explicit reason the domain cannot be compared; `None` when it can.
    pub unavailable_reason: Option<String>,
    pub metrics: Vec<DomainMetric>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessDifference {
    /// Normalized identity used for matching (lowercased, extension stripped).
    pub identity: String,
    pub display_name: String,
    pub a_cpu_median_pct: Option<f64>,
    pub b_cpu_median_pct: Option<f64>,
    pub delta: Option<f64>,
    /// "both" | "a_only" | "b_only"
    pub presence: &'static str,
    pub a_presence: usize,
    pub b_presence: usize,
    pub a_ticks: usize,
    pub b_ticks: usize,
    /// More than one executable matched this identity on that side.
    pub a_ambiguous: bool,
    pub b_ambiguous: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcessComparison {
    pub a_ticks: usize,
    pub b_ticks: usize,
    pub a_incomplete: Option<bool>,
    pub b_incomplete: Option<bool>,
    pub a_total_threads: Option<u64>,
    pub b_total_threads: Option<u64>,
    /// True when either side's enumeration was incomplete: an absent process
    /// is then "not observed", never "did not run".
    pub incomplete: bool,
    pub rows: Vec<ProcessDifference>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CategoricalDifference {
    pub key: &'static str,
    pub label: &'static str,
    pub a: Option<String>,
    pub b: Option<String>,
    /// "same" | "changed" | "unavailable"
    pub state: &'static str,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CorrelationComparison {
    pub x: &'static str,
    pub y: &'static str,
    pub label: String,
    pub r_a: Option<f64>,
    pub r_b: Option<f64>,
    pub n_a: usize,
    pub n_b: usize,
    pub effective_n_a: usize,
    pub effective_n_b: usize,
    /// Both coefficients exist and each side had enough evidence.
    pub comparable: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Caveat {
    /// Affected scope: a domain, "comparison", "coverage", "cadence", ...
    pub scope: &'static str,
    /// "info" | "warning" | "critical"
    pub severity: &'static str,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Energy, quality, side summaries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct EnergyComparison {
    pub total_wh_a: Option<f64>,
    pub total_wh_b: Option<f64>,
    pub avg_power_w_a: Option<f64>,
    pub avg_power_w_b: Option<f64>,
    /// Energy normalized to one hour on each side's own observed span.
    pub normalized_wh_a: Option<f64>,
    pub normalized_wh_b: Option<f64>,
    pub duration_ratio: Option<f64>,
    pub durations_similar: bool,
    /// "total_energy" for similar durations, "average_power" otherwise.
    pub preferred_basis: &'static str,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QualityComparison {
    pub coverage_a: Option<f64>,
    pub coverage_b: Option<f64>,
    pub discontinuities_a: usize,
    pub discontinuities_b: usize,
    pub timeouts_a: usize,
    pub timeouts_b: usize,
    pub stale_collectors_a: Vec<String>,
    pub stale_collectors_b: Vec<String>,
    /// "A" | "B" | "equal" | "unknown"
    pub weakest_side: &'static str,
    pub note: String,
}

/// Non-numeric context for one side's range. Rendered as Same/Changed/
/// Unavailable, never as a numeric delta.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RangeContext {
    pub power_scheme: Option<String>,
    pub power_source: Option<String>,
    pub refresh_hz: Option<String>,
    pub display_count: Option<usize>,
    pub gpu_adapters: Vec<String>,
    pub effective_cadence_ms: Option<f64>,
}

/// Public per-side summary: what range was actually compared and how good its
/// evidence is.
#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonSide {
    pub from_ms: u64,
    pub to_ms: u64,
    pub duration_s: f64,
    pub whole_session: bool,
    pub interval_ms: u64,
    pub coverage: Option<f64>,
    pub quality: RangeQuality,
    pub energy: RangeEnergy,
    pub context: RangeContext,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonAnalysis {
    pub a: ComparisonSide,
    pub b: ComparisonSide,
    pub comparability: Comparability,
    pub quality: QualityComparison,
    pub energy: EnergyComparison,
    pub headline_metrics: Vec<MetricComparison>,
    pub ranked_changes: Vec<RankedDifference>,
    pub domains: Vec<DomainComparison>,
    pub processes: ProcessComparison,
    pub categorical_differences: Vec<CategoricalDifference>,
    pub distributions: Vec<DistributionComparison>,
    pub correlations: Vec<CorrelationComparison>,
    pub caveats: Vec<Caveat>,
}

// ---------------------------------------------------------------------------
// Metric catalogue
// ---------------------------------------------------------------------------

struct HeadlineDef {
    key: &'static str,
    domain: &'static str,
    unit: &'static str,
    /// Absolute magnitude below which a difference is called unchanged.
    threshold_abs: f64,
    /// Whether a relative change is semantically meaningful for this metric.
    relative_suitable: bool,
}

const HEADLINE: &[HeadlineDef] = &[
    HeadlineDef {
        key: "battery_discharge_w",
        domain: "power",
        unit: "W",
        threshold_abs: 0.5,
        relative_suitable: true,
    },
    HeadlineDef {
        key: "battery_pct",
        domain: "battery",
        unit: "%",
        threshold_abs: 1.0,
        relative_suitable: false,
    },
    HeadlineDef {
        key: "cpu_utility_pct",
        domain: "cpu",
        unit: "%",
        threshold_abs: 5.0,
        relative_suitable: false,
    },
    HeadlineDef {
        key: "cpu_pkg_power_w",
        domain: "cpu",
        unit: "W",
        threshold_abs: 0.5,
        relative_suitable: true,
    },
    HeadlineDef {
        key: "cpu_pkg_derived_w",
        domain: "cpu",
        unit: "W",
        threshold_abs: 0.5,
        relative_suitable: true,
    },
    HeadlineDef {
        key: "cpu_c3_pct",
        domain: "cpu",
        unit: "%",
        threshold_abs: 10.0,
        relative_suitable: false,
    },
    HeadlineDef {
        key: "gpu_util_pct",
        domain: "gpu",
        unit: "%",
        threshold_abs: 5.0,
        relative_suitable: false,
    },
    HeadlineDef {
        key: "display_brightness_pct",
        domain: "display",
        unit: "%",
        threshold_abs: 5.0,
        relative_suitable: false,
    },
    HeadlineDef {
        key: "net_rx_bps",
        domain: "network",
        unit: "B/s",
        threshold_abs: 64.0 * 1024.0,
        relative_suitable: true,
    },
    HeadlineDef {
        key: "net_tx_bps",
        domain: "network",
        unit: "B/s",
        threshold_abs: 64.0 * 1024.0,
        relative_suitable: true,
    },
    HeadlineDef {
        key: "storage_disk_time_pct",
        domain: "storage",
        unit: "%",
        threshold_abs: 5.0,
        relative_suitable: false,
    },
    HeadlineDef {
        key: "storage_read_bps",
        domain: "storage",
        unit: "B/s",
        threshold_abs: 256.0 * 1024.0,
        relative_suitable: true,
    },
    HeadlineDef {
        key: "storage_write_bps",
        domain: "storage",
        unit: "B/s",
        threshold_abs: 256.0 * 1024.0,
        relative_suitable: true,
    },
];

const MIN_EFFECTIVE_N: usize = 5;
const WEAK_COVERAGE: f64 = 0.5;
const LOW_COVERAGE: f64 = 0.7;
const CADENCE_RATIO_CAVEAT: f64 = 3.0;
const DURATION_RATIO_CAVEAT: f64 = 1.5;

/// Curated comparison metric keys, in the same order Compare uses. Experiment
/// secondary analysis consumes this list so Analyze, Compare and Experiments
/// can never disagree about which metrics are analytically useful.
pub(crate) fn curated_metric_keys() -> Vec<&'static str> {
    HEADLINE.iter().map(|d| d.key).collect()
}

fn find_metric<'a>(domains: &'a [DomainStat], key: &str) -> Option<&'a MetricStat> {
    domains
        .iter()
        .flat_map(|d| d.metrics.iter())
        .find(|m| m.key == key)
}

fn spread_of(stat: &Stat) -> Option<f64> {
    match (stat.p10, stat.p90) {
        (Some(a), Some(b)) => Some((b - a).abs()),
        _ => None,
    }
}

fn in_range(p: &Sample, from_ms: u64, to_ms: u64) -> bool {
    p.wall_ms >= from_ms && p.wall_ms <= to_ms
}

fn window_samples(m: &MetricSeries, from_ms: u64, to_ms: u64) -> Vec<Sample> {
    m.samples
        .iter()
        .copied()
        .filter(|p| in_range(p, from_ms, to_ms))
        .collect()
}

struct ReliabilityInput {
    effective_n: usize,
    known: usize,
    spread: Option<f64>,
    coverage: Option<f64>,
}

fn reliability_input(m: Option<&MetricSeries>, from_ms: u64, to_ms: u64) -> ReliabilityInput {
    let Some(m) = m else {
        return ReliabilityInput {
            effective_n: 0,
            known: 0,
            spread: None,
            coverage: None,
        };
    };
    let window = window_samples(m, from_ms, to_ms);
    let vals: Vec<f64> = window.iter().filter_map(|p| p.value).collect();
    let stat = Stat::of(&window);
    let unique = unique_count(&vals);
    let rho = stats::lag1_autocorr(&vals).abs();
    ReliabilityInput {
        effective_n: stats::effective_n(vals.len(), unique, rho),
        known: vals.len(),
        spread: spread_of(&stat),
        coverage: stat.coverage(),
    }
}

fn unique_count(vals: &[f64]) -> usize {
    let mut seen: Vec<i64> = vals.iter().map(|v| (v * 100.0).round() as i64).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

fn provenance_label(p: &str) -> &'static str {
    match p {
        "measured" => "measured",
        "derived" => "derived",
        "estimated" => "estimated",
        _ => "unavailable",
    }
}

fn evidence_line(pa: &'static str, pb: &'static str) -> String {
    let a = provenance_label(pa);
    let b = provenance_label(pb);
    if a == b {
        format!("{a} on both sides")
    } else {
        format!("{a} on A, {b} on B")
    }
}

// ---------------------------------------------------------------------------
// Top-level comparison
// ---------------------------------------------------------------------------

/// Compare two ranges (or whole sessions) deterministically and
/// observationally. Both sides are analyzed through [`analyze::analyze_range`]
/// so quality, energy, domain and process evidence can never drift between the
/// Analyze and Compare workspaces.
pub fn compare_ranges(a: ComparisonInput<'_>, b: ComparisonInput<'_>) -> ComparisonAnalysis {
    let (a_from, a_to) = a.resolved_range();
    let (b_from, b_to) = b.resolved_range();
    let a_analysis = analyze::analyze_range(a.session, a.interval_ms, a_from, a_to);
    let b_analysis = analyze::analyze_range(b.session, b.interval_ms, b_from, b_to);
    let a_series = analyze::metric_series(a.session);
    let b_series = analyze::metric_series(b.session);
    let a_ctx = range_context(a.session, a_from, a_to);
    let b_ctx = range_context(b.session, b_from, b_to);

    let side = |from: u64,
                to: u64,
                whole: bool,
                interval_ms: u64,
                analysis: &analyze::RangeAnalysis,
                context: RangeContext| ComparisonSide {
        from_ms: from,
        to_ms: to,
        duration_s: (to.saturating_sub(from)) as f64 / 1000.0,
        whole_session: whole,
        interval_ms,
        coverage: analysis.quality.coverage,
        quality: analysis.quality.clone(),
        energy: analysis.energy.clone(),
        context,
    };

    let comparison = ComparisonAnalysis {
        a: side(
            a_from,
            a_to,
            a.from_ms.is_none() && a.to_ms.is_none(),
            a.interval_ms,
            &a_analysis,
            a_ctx.clone(),
        ),
        b: side(
            b_from,
            b_to,
            b.from_ms.is_none() && b.to_ms.is_none(),
            b.interval_ms,
            &b_analysis,
            b_ctx.clone(),
        ),
        comparability: Comparability {
            overall: ComparabilityLevel::Compatible,
            findings: Vec::new(),
        },
        quality: QualityComparison {
            coverage_a: None,
            coverage_b: None,
            discontinuities_a: 0,
            discontinuities_b: 0,
            timeouts_a: 0,
            timeouts_b: 0,
            stale_collectors_a: Vec::new(),
            stale_collectors_b: Vec::new(),
            weakest_side: "unknown",
            note: String::new(),
        },
        energy: EnergyComparison {
            total_wh_a: None,
            total_wh_b: None,
            avg_power_w_a: None,
            avg_power_w_b: None,
            normalized_wh_a: None,
            normalized_wh_b: None,
            duration_ratio: None,
            durations_similar: false,
            preferred_basis: "average_power",
            note: String::new(),
        },
        headline_metrics: Vec::new(),
        ranked_changes: Vec::new(),
        domains: Vec::new(),
        processes: ProcessComparison {
            a_ticks: 0,
            b_ticks: 0,
            a_incomplete: None,
            b_incomplete: None,
            a_total_threads: None,
            b_total_threads: None,
            incomplete: false,
            rows: Vec::new(),
        },
        categorical_differences: Vec::new(),
        distributions: Vec::new(),
        correlations: Vec::new(),
        caveats: Vec::new(),
    };

    build(
        a,
        b,
        &a_analysis,
        &b_analysis,
        &a_series,
        &b_series,
        comparison,
    )
}

#[allow(clippy::too_many_arguments)]
fn build(
    a: ComparisonInput<'_>,
    b: ComparisonInput<'_>,
    a_analysis: &analyze::RangeAnalysis,
    b_analysis: &analyze::RangeAnalysis,
    a_series: &[MetricSeries],
    b_series: &[MetricSeries],
    mut out: ComparisonAnalysis,
) -> ComparisonAnalysis {
    let a_from = a_analysis.from_ms;
    let a_to = a_analysis.to_ms;
    let b_from = b_analysis.from_ms;
    let b_to = b_analysis.to_ms;
    let a_domains = &a_analysis.domains;
    let b_domains = &b_analysis.domains;

    // --- quality -----------------------------------------------------------
    let coverage_a = a_analysis.quality.coverage;
    let coverage_b = b_analysis.quality.coverage;
    let weak_a = coverage_a.is_some_and(|c| c < WEAK_COVERAGE);
    let weak_b = coverage_b.is_some_and(|c| c < WEAK_COVERAGE);
    out.quality = QualityComparison {
        coverage_a,
        coverage_b,
        discontinuities_a: a_analysis.quality.discontinuities,
        discontinuities_b: b_analysis.quality.discontinuities,
        timeouts_a: a_analysis.quality.timeouts,
        timeouts_b: b_analysis.quality.timeouts,
        stale_collectors_a: a_analysis
            .quality
            .stale_collectors
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        stale_collectors_b: b_analysis
            .quality
            .stale_collectors
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        weakest_side: match (coverage_a, coverage_b) {
            (Some(x), Some(y)) if (x - y).abs() < 1e-9 => "equal",
            (Some(x), Some(y)) if x < y => "A",
            (Some(_), Some(_)) => "B",
            (Some(_), None) => "A",
            (None, Some(_)) => "B",
            (None, None) => "unknown",
        },
        note: String::new(),
    };
    out.quality.note = format!(
        "Coverage A {} / B {}; quality differences are metric-specific below.",
        coverage_a
            .map(|c| format!("{:.0}%", c * 100.0))
            .unwrap_or_else(|| "unknown".into()),
        coverage_b
            .map(|c| format!("{:.0}%", c * 100.0))
            .unwrap_or_else(|| "unknown".into()),
    );

    // --- energy normalization ---------------------------------------------
    let dur_a = (a_to.saturating_sub(a_from)) as f64 / 1000.0;
    let dur_b = (b_to.saturating_sub(b_from)) as f64 / 1000.0;
    out.energy = energy_comparison(&a_analysis.energy, &b_analysis.energy, dur_a, dur_b);

    // --- comparability (comparison-wide) ----------------------------------
    let mut findings: Vec<ComparabilityFinding> = Vec::new();
    let observed_a = observed_collectors(a.session);
    let observed_b = observed_collectors(b.session);
    if observed_a != observed_b {
        findings.push(ComparabilityFinding {
            metric: None,
            level: ComparabilityLevel::CompatibleWithCaveats,
            reason: format!(
                "different collectors recorded (A: {}; B: {})",
                join_or(&observed_a),
                join_or(&observed_b)
            ),
        });
    }
    if weak_a || weak_b {
        findings.push(ComparabilityFinding {
            metric: None,
            level: ComparabilityLevel::Weak,
            reason: format!(
                "low discharge coverage (A {}, B {})",
                coverage_a
                    .map(|c| format!("{:.0}%", c * 100.0))
                    .unwrap_or_else(|| "unknown".into()),
                coverage_b
                    .map(|c| format!("{:.0}%", c * 100.0))
                    .unwrap_or_else(|| "unknown".into()),
            ),
        });
    }
    if let (Some(ca), Some(cb)) = (
        out.a.context.effective_cadence_ms,
        out.b.context.effective_cadence_ms,
    ) && ca > 0.0
        && cb > 0.0
    {
        let ratio = (ca / cb).max(cb / ca);
        if ratio > CADENCE_RATIO_CAVEAT {
            findings.push(ComparabilityFinding {
                metric: None,
                level: ComparabilityLevel::CompatibleWithCaveats,
                reason: format!(
                    "effective cadence differs by {ratio:.1}x (A {:.0} ms, B {:.0} ms)",
                    ca, cb
                ),
            });
        }
    }
    if let (Some(sa), Some(sb)) = (
        out.a.context.power_source.as_deref(),
        out.b.context.power_source.as_deref(),
    ) && sa != sb
    {
        findings.push(ComparabilityFinding {
            metric: Some("battery_discharge_w"),
            level: ComparabilityLevel::NotComparable,
            reason: format!(
                "different power source (A {sa}, B {sb}); power/battery metrics are not comparable"
            ),
        });
    }
    if dur_a > 0.0 && dur_b > 0.0 {
        let ratio = (dur_a / dur_b).max(dur_b / dur_a);
        if ratio > DURATION_RATIO_CAVEAT {
            findings.push(ComparabilityFinding {
                metric: None,
                level: ComparabilityLevel::CompatibleWithCaveats,
                reason: format!(
                    "durations differ substantially ({:.0}s vs {:.0}s, {ratio:.1}x)",
                    dur_a, dur_b
                ),
            });
        }
    }
    if let Some(recovered) = session_recovered(a.session).or_else(|| session_recovered(b.session)) {
        let which = if session_recovered(a.session).is_some() {
            "A"
        } else {
            "B"
        };
        if recovered {
            findings.push(ComparabilityFinding {
                metric: None,
                level: ComparabilityLevel::CompatibleWithCaveats,
                reason: format!(
                    "session {which} was recovered; recording does not contain a clean footer"
                ),
            });
        }
    }

    // --- headline metrics --------------------------------------------------
    let mut metrics: Vec<MetricComparison> = Vec::new();
    for def in HEADLINE {
        let ma = find_metric(a_domains, def.key);
        let mb = find_metric(b_domains, def.key);
        let a_known = ma.map(|m| m.stat.known).unwrap_or(0);
        let b_known = mb.map(|m| m.stat.known).unwrap_or(0);
        if a_known == 0 && b_known == 0 {
            continue;
        }
        let ra = reliability_input(a_series.iter().find(|m| m.key == def.key), a_from, a_to);
        let rb = reliability_input(b_series.iter().find(|m| m.key == def.key), b_from, b_to);
        let cmp = compare_metric(def, ma, mb, &ra, &rb, &out);
        findings.push(ComparabilityFinding {
            metric: Some(def.key),
            level: cmp.comparability,
            reason: cmp.comparability_reason.clone(),
        });
        metrics.push(cmp);
    }
    out.headline_metrics = metrics;

    // --- ranked differences ------------------------------------------------
    let mut ranked: Vec<RankedDifference> = out
        .headline_metrics
        .iter()
        .filter(|m| m.comparability != ComparabilityLevel::NotComparable)
        .filter(|m| m.reliability.level != "insufficient" && m.absolute_delta.is_some())
        .map(|m| {
            let delta = m.absolute_delta.unwrap_or(0.0);
            let noise = m.reliability.noise_floor.unwrap_or(0.0).max(1e-9);
            RankedDifference {
                metric: m.metric,
                label: m.label,
                domain: m.domain,
                unit: m.unit,
                delta,
                relative_delta: m.relative_delta,
                direction: m.direction,
                relevance: delta.abs() / noise,
                comparability: m.comparability,
                reliability: m.reliability.level,
                basis: m.evidence.clone(),
            }
        })
        .collect();
    ranked.sort_by(|x, y| {
        y.relevance
            .total_cmp(&x.relevance)
            .then_with(|| y.delta.abs().total_cmp(&x.delta.abs()))
            .then_with(|| x.label.cmp(y.label))
    });
    out.ranked_changes = ranked;

    // --- distributions -----------------------------------------------------
    out.distributions = HEADLINE
        .iter()
        .filter_map(|def| {
            let ma = find_metric(a_domains, def.key);
            let mb = find_metric(b_domains, def.key);
            let a = DistributionSummary::of(ma);
            let b = DistributionSummary::of(mb);
            if a.known == 0 && b.known == 0 {
                return None;
            }
            Some(DistributionComparison {
                metric: def.key,
                label: ma.or(mb).map(|m| m.label).unwrap_or(def.key),
                unit: def.unit,
                a,
                b,
            })
        })
        .collect();

    // --- domains -----------------------------------------------------------
    out.domains = domain_comparisons(a_domains, b_domains);

    // --- processes ---------------------------------------------------------
    out.processes = process_comparison(&a_analysis.processes, &b_analysis.processes);

    // --- categorical -------------------------------------------------------
    out.categorical_differences = categorical_differences(
        &out.a.context,
        &out.b.context,
        &out.a.quality,
        &out.b.quality,
    );

    // --- correlations ------------------------------------------------------
    out.correlations = correlation_comparison(&a_analysis.correlations, &b_analysis.correlations);

    // --- caveats -----------------------------------------------------------
    out.caveats = caveats(
        &out.a,
        &out.b,
        &out.energy,
        &out.processes,
        &out.headline_metrics,
    );

    // --- overall comparability --------------------------------------------
    let mut overall = ComparabilityLevel::Compatible;
    for f in &findings {
        overall = overall.max(f.level);
    }
    // A comparison where every metric is unavailable in one direction is not
    // meaningfully comparable even without a global finding.
    if !out.headline_metrics.is_empty()
        && out
            .headline_metrics
            .iter()
            .all(|m| m.comparability == ComparabilityLevel::NotComparable)
    {
        overall = ComparabilityLevel::NotComparable;
    }
    findings.retain(|f| f.metric.is_none() || f.level != ComparabilityLevel::Compatible);
    findings.sort_by(|x, y| {
        y.level
            .rank()
            .cmp(&x.level.rank())
            .then_with(|| x.reason.cmp(&y.reason))
    });
    out.comparability = Comparability { overall, findings };
    out
}

fn compare_metric(
    def: &HeadlineDef,
    ma: Option<&MetricStat>,
    mb: Option<&MetricStat>,
    ra: &ReliabilityInput,
    rb: &ReliabilityInput,
    out: &ComparisonAnalysis,
) -> MetricComparison {
    let a_known = ma.map(|m| m.stat.known).unwrap_or(0);
    let b_known = mb.map(|m| m.stat.known).unwrap_or(0);
    let label = ma.or(mb).map(|m| m.label).unwrap_or(def.key);
    let provenance_a = ma.map(|m| m.provenance).unwrap_or("unavailable");
    let provenance_b = mb.map(|m| m.provenance).unwrap_or("unavailable");
    let a = ma.and_then(|m| m.stat.median);
    let b = mb.and_then(|m| m.stat.median);
    let delta = match (a, b) {
        (Some(x), Some(y)) => Some(y - x),
        _ => None,
    };

    // Comparability can differ by metric: this is the core of the gate. The
    // power/battery context checks run before the generic "missing evidence"
    // branch so a charging side is named as the reason, not merely "missing".
    let is_power = def.domain == "power" || def.domain == "battery";
    let charging = is_power && charge_mismatch(def.key, &out.a, &out.b);
    let (comparability, reason) = if a_known == 0 && b_known == 0 {
        (
            ComparabilityLevel::NotComparable,
            "no evidence on either side".to_string(),
        )
    } else if is_power && power_source_mismatch(&out.a.context, &out.b.context) {
        (
            ComparabilityLevel::NotComparable,
            "different power source (AC vs battery) makes power/battery deltas incomparable"
                .to_string(),
        )
    } else if charging {
        (
            ComparabilityLevel::NotComparable,
            "one side was charging while the other was discharging; discharge power is not comparable"
                .to_string(),
        )
    } else if a_known == 0 {
        (
            ComparabilityLevel::NotComparable,
            format!("missing evidence in A for {}", label),
        )
    } else if b_known == 0 {
        (
            ComparabilityLevel::NotComparable,
            format!("missing evidence in B for {}", label),
        )
    } else {
        coverage_level(def, ma, mb)
    };

    let reliability = if comparability == ComparabilityLevel::NotComparable {
        Reliability {
            level: "insufficient",
            note: "no defensible comparison for this metric".to_string(),
            noise_floor: None,
            effect_size: None,
        }
    } else {
        reliability(def, delta, ra, rb)
    };

    let direction = if a_known == 0 && b_known == 0 {
        "unavailable"
    } else if a_known == 0 {
        "missing_a"
    } else if b_known == 0 {
        "missing_b"
    } else {
        match delta {
            Some(d) if d.abs() < def.threshold_abs => "unchanged",
            Some(d) if d > 0.0 => "increased",
            Some(_) => "decreased",
            None => "unavailable",
        }
    };

    // Never emit a percentage against a zero, near-zero or unsuitable base.
    let relative_delta = delta.and_then(|d| match (a, def.relative_suitable) {
        (Some(base), true) if base.abs() >= def.threshold_abs => Some(d / base),
        _ => None,
    });

    MetricComparison {
        metric: def.key,
        label,
        domain: def.domain,
        unit: def.unit,
        a,
        b,
        absolute_delta: delta,
        relative_delta,
        direction,
        sample_count_a: ra.known,
        sample_count_b: rb.known,
        coverage_a: ra.coverage,
        coverage_b: rb.coverage,
        provenance_a,
        provenance_b,
        comparability,
        comparability_reason: reason,
        evidence: evidence_line(provenance_a, provenance_b),
        reliability,
    }
}

fn coverage_level(
    _def: &HeadlineDef,
    ma: Option<&MetricStat>,
    mb: Option<&MetricStat>,
) -> (ComparabilityLevel, String) {
    let cov = |m: Option<&MetricStat>| m.and_then(|m| m.stat.coverage());
    let ca = cov(ma);
    let cb = cov(mb);
    match (ca, cb) {
        (Some(x), Some(y)) if x < WEAK_COVERAGE || y < WEAK_COVERAGE => (
            ComparabilityLevel::Weak,
            format!(
                "weak sample coverage in range (A {:.0}%, B {:.0}%)",
                x * 100.0,
                y * 100.0
            ),
        ),
        (Some(x), Some(y)) if x < LOW_COVERAGE || y < LOW_COVERAGE => (
            ComparabilityLevel::CompatibleWithCaveats,
            format!(
                "partial sample coverage in range (A {:.0}%, B {:.0}%)",
                x * 100.0,
                y * 100.0
            ),
        ),
        _ => (
            ComparabilityLevel::Compatible,
            "comparable evidence on both sides".to_string(),
        ),
    }
}

fn power_source_mismatch(a: &RangeContext, b: &RangeContext) -> bool {
    match (a.power_source.as_deref(), b.power_source.as_deref()) {
        (Some(x), Some(y)) => x != y,
        _ => false,
    }
}

fn charge_mismatch(key: &str, a: &ComparisonSide, b: &ComparisonSide) -> bool {
    if key != "battery_discharge_w" {
        return false;
    }
    let charging = |s: &ComparisonSide| s.energy.charge_present && !s.energy.discharge_present;
    let discharging = |s: &ComparisonSide| s.energy.discharge_present && !s.energy.charge_present;
    (charging(a) && discharging(b)) || (discharging(a) && charging(b))
}

fn reliability(
    def: &HeadlineDef,
    delta: Option<f64>,
    ra: &ReliabilityInput,
    rb: &ReliabilityInput,
) -> Reliability {
    let eff = ra.effective_n.min(rb.effective_n);
    let coverage = match (ra.coverage, rb.coverage) {
        (Some(x), Some(y)) => Some(x.min(y)),
        _ => None,
    };
    let spread = match (ra.spread, rb.spread) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };
    let noise = def.threshold_abs.max(spread.unwrap_or(0.0));
    let noise_floor = Some(noise);
    let effect_size = match (delta, spread) {
        (Some(d), Some(s)) if s > 0.0 => Some(d.abs() / s),
        _ => None,
    };
    let Some(delta) = delta else {
        return Reliability {
            level: "insufficient",
            note: "missing median on at least one side".to_string(),
            noise_floor,
            effect_size,
        };
    };
    let weak_coverage = coverage.is_some_and(|c| c < WEAK_COVERAGE);
    let (level, note) = if eff < MIN_EFFECTIVE_N || weak_coverage {
        (
            "insufficient",
            format!(
                "effective n {eff} (min {MIN_EFFECTIVE_N}), coverage {}",
                coverage
                    .map(|c| format!("{:.0}%", c * 100.0))
                    .unwrap_or_else(|| "unknown".into())
            ),
        )
    } else if delta.abs() < noise {
        (
            "within_noise",
            format!(
                "|delta| {:.3} is within the measured noise floor {noise:.3}",
                delta.abs()
            ),
        )
    } else if eff < 10 {
        (
            "weak_evidence",
            format!("difference exceeds the noise floor but rests on effective n {eff}"),
        )
    } else {
        (
            "distinguishable",
            format!("difference exceeds the noise floor with effective n {eff}"),
        )
    };
    Reliability {
        level,
        note,
        noise_floor,
        effect_size,
    }
}

// ---------------------------------------------------------------------------
// Energy
// ---------------------------------------------------------------------------

fn energy_comparison(a: &RangeEnergy, b: &RangeEnergy, dur_a: f64, dur_b: f64) -> EnergyComparison {
    let avg = |e: &RangeEnergy| {
        e.discharge_wh
            .filter(|_| e.covered_s > 0.0)
            .map(|wh| wh / (e.covered_s / 3600.0))
    };
    let normalized = |e: &RangeEnergy, dur: f64| {
        e.discharge_wh
            .filter(|_| dur > 0.0)
            .map(|wh| wh / (dur / 3600.0))
    };
    let a_total = a.discharge_wh;
    let b_total = b.discharge_wh;
    let duration_ratio = (dur_a > 0.0 && dur_b > 0.0).then(|| (dur_a / dur_b).max(dur_b / dur_a));
    let durations_similar = duration_ratio.is_none_or(|r| r <= DURATION_RATIO_CAVEAT);
    let (preferred_basis, note) = if durations_similar {
        (
            "total_energy",
            "Durations are similar; total discharge energy is directly meaningful. Average power is shown for context.".to_string(),
        )
    } else {
        (
            "average_power",
            format!(
                "Durations differ {:.0}s vs {:.0}s; average power and per-hour energy are the honest basis, not raw Wh.",
                dur_a, dur_b
            ),
        )
    };
    EnergyComparison {
        total_wh_a: a_total,
        total_wh_b: b_total,
        avg_power_w_a: avg(a),
        avg_power_w_b: avg(b),
        normalized_wh_a: normalized(a, dur_a),
        normalized_wh_b: normalized(b, dur_b),
        duration_ratio,
        durations_similar,
        preferred_basis,
        note,
    }
}

// ---------------------------------------------------------------------------
// Domains
// ---------------------------------------------------------------------------

fn domain_comparisons(a: &[DomainStat], b: &[DomainStat]) -> Vec<DomainComparison> {
    a.iter()
        .map(|da| {
            let db = b.iter().find(|d| d.domain == da.domain);
            let available_a = da.available;
            let available_b = db.map(|d| d.available).unwrap_or(false);
            let metrics = da
                .metrics
                .iter()
                .filter_map(|ma| {
                    let mb = db.and_then(|d| d.metrics.iter().find(|m| m.key == ma.key));
                    let a_known = ma.stat.known > 0;
                    let b_known = mb.map(|m| m.stat.known > 0).unwrap_or(false);
                    if !a_known && !b_known {
                        return None;
                    }
                    let delta = match (ma.stat.median, mb.and_then(|m| m.stat.median)) {
                        (Some(x), Some(y)) => Some(y - x),
                        _ => None,
                    };
                    Some(DomainMetric {
                        metric: ma.key,
                        label: ma.label,
                        unit: ma.unit,
                        a_median: ma.stat.median,
                        b_median: mb.and_then(|m| m.stat.median),
                        delta,
                        known_a: ma.stat.known,
                        known_b: mb.map(|m| m.stat.known).unwrap_or(0),
                    })
                })
                .collect();
            let unavailable_reason = if !available_a && !available_b {
                Some(format!("no {} evidence recorded on either side", da.domain))
            } else if !available_a {
                Some(format!(
                    "not comparable — missing {} evidence in A",
                    da.domain
                ))
            } else if !available_b {
                Some(format!(
                    "not comparable — missing {} evidence in B",
                    da.domain
                ))
            } else {
                None
            };
            DomainComparison {
                domain: da.domain,
                available_a,
                available_b,
                unavailable_reason,
                metrics,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

fn normalize_name(name: &str) -> String {
    let n = name.trim().to_ascii_lowercase();
    n.strip_suffix(".exe").unwrap_or(&n).to_string()
}

#[derive(Default)]
struct ProcAgg {
    display: String,
    pids: Vec<u32>,
    cpu_max: Option<f64>,
    presence: usize,
    ticks: usize,
}

fn aggregate_processes(pa: &ProcessAnalysis) -> BTreeMap<String, ProcAgg> {
    let mut map: BTreeMap<String, ProcAgg> = BTreeMap::new();
    for e in &pa.entries {
        let key = normalize_name(&e.name);
        let agg = map.entry(key).or_default();
        if agg.display.is_empty() {
            agg.display = e.name.clone();
        }
        if !agg.pids.contains(&e.pid) {
            agg.pids.push(e.pid);
        }
        if let Some(v) = e.cpu_median_pct {
            agg.cpu_max = Some(agg.cpu_max.map_or(v, |c| c.max(v)));
        }
        agg.presence += e.presence;
        agg.ticks = agg.ticks.max(e.ticks);
    }
    map
}

fn process_comparison(a: &ProcessAnalysis, b: &ProcessAnalysis) -> ProcessComparison {
    let a_map = aggregate_processes(a);
    let b_map = aggregate_processes(b);
    let mut keys: Vec<String> = a_map.keys().chain(b_map.keys()).cloned().collect();
    keys.sort();
    keys.dedup();

    let incomplete = a.incomplete == Some(true) || b.incomplete == Some(true);
    let mut rows: Vec<ProcessDifference> = keys
        .into_iter()
        .map(|key| {
            let ea = a_map.get(&key);
            let eb = b_map.get(&key);
            let a_cpu = ea.and_then(|e| e.cpu_max);
            let b_cpu = eb.and_then(|e| e.cpu_max);
            let delta = match (a_cpu, b_cpu) {
                (Some(x), Some(y)) => Some(y - x),
                _ => None,
            };
            let presence = match (ea.is_some(), eb.is_some()) {
                (true, true) => "both",
                (true, false) => "a_only",
                _ => "b_only",
            };
            let note = match presence {
                "a_only" if incomplete => "not observed in B (evidence incomplete)".to_string(),
                "a_only" => "observed only in A".to_string(),
                "b_only" if incomplete => "not observed in A (evidence incomplete)".to_string(),
                "b_only" => "observed only in B".to_string(),
                _ => "observed on both sides".to_string(),
            };
            ProcessDifference {
                identity: key,
                display_name: ea.or(eb).map(|e| e.display.clone()).unwrap_or_default(),
                a_cpu_median_pct: a_cpu,
                b_cpu_median_pct: b_cpu,
                delta,
                presence,
                a_presence: ea.map(|e| e.presence).unwrap_or(0),
                b_presence: eb.map(|e| e.presence).unwrap_or(0),
                a_ticks: ea.map(|e| e.ticks).unwrap_or(a.ticks),
                b_ticks: eb.map(|e| e.ticks).unwrap_or(b.ticks),
                a_ambiguous: ea.map(|e| e.pids.len() > 1).unwrap_or(false),
                b_ambiguous: eb.map(|e| e.pids.len() > 1).unwrap_or(false),
                note,
            }
        })
        .collect();
    rows.sort_by(|x, y| {
        let x_both = x.presence == "both";
        let y_both = y.presence == "both";
        y_both
            .cmp(&x_both)
            .then_with(|| {
                y.delta
                    .unwrap_or(0.0)
                    .abs()
                    .total_cmp(&x.delta.unwrap_or(0.0).abs())
            })
            .then_with(|| x.identity.cmp(&y.identity))
    });
    rows.truncate(200);
    ProcessComparison {
        a_ticks: a.ticks,
        b_ticks: b.ticks,
        a_incomplete: a.incomplete,
        b_incomplete: b.incomplete,
        a_total_threads: a.total_threads,
        b_total_threads: b.total_threads,
        incomplete,
        rows,
    }
}

// ---------------------------------------------------------------------------
// Categorical context
// ---------------------------------------------------------------------------

fn categorical_differences(
    a: &RangeContext,
    b: &RangeContext,
    qa: &RangeQuality,
    qb: &RangeQuality,
) -> Vec<CategoricalDifference> {
    let mut out = Vec::new();
    let mut push = |key: &'static str,
                    label: &'static str,
                    av: Option<String>,
                    bv: Option<String>,
                    same_note: &str| {
        let (state, note) = match (&av, &bv) {
            (Some(x), Some(y)) if x == y => ("same", same_note.to_string()),
            (Some(x), Some(y)) => ("changed", format!("{x} → {y}")),
            _ => (
                "unavailable",
                "not recorded on one or both sides".to_string(),
            ),
        };
        out.push(CategoricalDifference {
            key,
            label,
            a: av,
            b: bv,
            state,
            note,
        });
    };
    push(
        "power_scheme",
        "Power scheme",
        a.power_scheme.clone(),
        b.power_scheme.clone(),
        "same scheme on both sides",
    );
    push(
        "power_source",
        "Power source",
        a.power_source.clone(),
        b.power_source.clone(),
        "same power source",
    );
    push(
        "refresh_hz",
        "Refresh rate",
        a.refresh_hz.clone(),
        b.refresh_hz.clone(),
        "same refresh rate",
    );
    push(
        "display_count",
        "Display count",
        a.display_count.map(|c| c.to_string()),
        b.display_count.map(|c| c.to_string()),
        "same number of displays",
    );
    push(
        "gpu_adapters",
        "GPU adapters",
        (!a.gpu_adapters.is_empty()).then(|| a.gpu_adapters.join(", ")),
        (!b.gpu_adapters.is_empty()).then(|| b.gpu_adapters.join(", ")),
        "same adapters recorded",
    );
    push(
        "effective_cadence",
        "Effective cadence",
        a.effective_cadence_ms.map(|c| format!("{c:.0} ms")),
        b.effective_cadence_ms.map(|c| format!("{c:.0} ms")),
        "same effective cadence",
    );
    push(
        "collectors",
        "Active collectors",
        Some(collector_set(qa)),
        Some(collector_set(qb)),
        "same collectors reported samples in range",
    );
    out
}

fn collector_set(q: &RangeQuality) -> String {
    let mut names: Vec<&str> = q.collector_counts.iter().map(|(n, _)| *n).collect();
    names.sort_unstable();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Correlations
// ---------------------------------------------------------------------------

fn correlation_comparison(a: &[Correlation], b: &[Correlation]) -> Vec<CorrelationComparison> {
    a.iter()
        .map(|ca| {
            let cb = b.iter().find(|c| c.x == ca.x && c.y == ca.y);
            let usable = |c: &Correlation| c.r.is_some() && c.strength != "insufficient";
            let a_ok = usable(ca);
            let b_ok = cb.is_some_and(usable);
            let comparable = a_ok && b_ok;
            let note = if comparable {
                "Both sides have defensible coefficients. Correlation is not causation.".to_string()
            } else if !a_ok && !b_ok {
                "Insufficient evidence on both sides".to_string()
            } else if !a_ok {
                "A has insufficient evidence for a coefficient".to_string()
            } else {
                "B has insufficient evidence for a coefficient".to_string()
            };
            CorrelationComparison {
                x: ca.x,
                y: ca.y,
                label: ca.label.clone(),
                r_a: ca.r,
                r_b: cb.and_then(|c| c.r),
                n_a: ca.n,
                n_b: cb.map(|c| c.n).unwrap_or(0),
                effective_n_a: ca.effective_n,
                effective_n_b: cb.map(|c| c.effective_n).unwrap_or(0),
                comparable,
                note,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Caveats
// ---------------------------------------------------------------------------

fn caveats(
    a: &ComparisonSide,
    b: &ComparisonSide,
    energy: &EnergyComparison,
    processes: &ProcessComparison,
    metrics: &[MetricComparison],
) -> Vec<Caveat> {
    let mut out: Vec<Caveat> = Vec::new();
    let mut push = |scope: &'static str, severity: &'static str, message: String| {
        out.push(Caveat {
            scope,
            severity,
            message,
        });
    };

    if a.coverage.is_some_and(|c| c < WEAK_COVERAGE)
        || b.coverage.is_some_and(|c| c < WEAK_COVERAGE)
    {
        push(
            "coverage",
            "critical",
            format!(
                "Low discharge coverage (A {}, B {}); any headline difference is qualified.",
                pct(a.coverage),
                pct(b.coverage)
            ),
        );
    } else if a.coverage.is_some_and(|c| c < LOW_COVERAGE)
        || b.coverage.is_some_and(|c| c < LOW_COVERAGE)
    {
        push(
            "coverage",
            "warning",
            format!(
                "Partial discharge coverage (A {}, B {}).",
                pct(a.coverage),
                pct(b.coverage)
            ),
        );
    }
    for (side, s) in [("A", a), ("B", b)] {
        if s.energy.discontinuities > 0 {
            push(
                "coverage",
                "warning",
                format!(
                    "Side {side} crosses {} clock discontinuity(ies); energy is integrated only over contiguous known segments.",
                    s.energy.discontinuities
                ),
            );
        }
        if !s.quality.stale_collectors.is_empty() {
            push(
                "coverage",
                "warning",
                format!(
                    "Side {side} has collectors with no samples in range: {}.",
                    s.quality.stale_collectors.join(", ")
                ),
            );
        }
        if s.quality.timeouts > 0 {
            push(
                "cadence",
                "info",
                format!(
                    "Side {side} recorded {} collector timeout(s) in range.",
                    s.quality.timeouts
                ),
            );
        }
    }
    if !energy.durations_similar {
        push("comparison", "warning", energy.note.clone());
    }
    if let (Some(ca), Some(cb)) = (
        a.context.effective_cadence_ms,
        b.context.effective_cadence_ms,
    ) && ca > 0.0
        && cb > 0.0
        && (ca / cb).max(cb / ca) > CADENCE_RATIO_CAVEAT
    {
        push(
            "cadence",
            "warning",
            format!(
                "Effective cadence differs {ca:.0} ms vs {cb:.0} ms; sample density is not equivalent."
            ),
        );
    }
    if processes.incomplete {
        push(
            "processes",
            "warning",
            "Process evidence is incomplete on at least one side; an absent process is \u{201c}not observed\u{201d}, not proof it did not run.".to_string(),
        );
    }
    let missing: Vec<&str> = metrics
        .iter()
        .filter(|m| m.comparability == ComparabilityLevel::NotComparable)
        .map(|m| m.label)
        .collect();
    if !missing.is_empty() {
        push(
            "comparison",
            "warning",
            format!("Not comparable for: {}.", missing.join(", ")),
        );
    }
    if a.energy.charge_present && b.energy.charge_present {
        push(
            "battery",
            "info",
            "Both sides recorded charging; discharge-only metrics exclude those intervals."
                .to_string(),
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Range context
// ---------------------------------------------------------------------------

fn observed_collectors(s: &SessionData) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut push = |name: &'static str, has: bool| {
        if has {
            out.push(name);
        }
    };
    push("battery", !s.battery.is_empty());
    push("cpu", !s.cpu.is_empty());
    push("gpu", !s.gpu.is_empty());
    push("proc", !s.procs.is_empty());
    push("display", !s.display.is_empty());
    push("net", !s.net.is_empty());
    push("storage", !s.storage.is_empty());
    push("usb", !s.usb.is_empty());
    push("self", !s.selfmon.is_empty());
    push("os_power", !s.policy_history.is_empty());
    out
}

fn join_or(items: &[&'static str]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

fn pct(v: Option<f64>) -> String {
    v.map(|c| format!("{:.0}%", c * 100.0))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Non-numeric context for a range, extracted from the authoritative session
/// model. Reuses the Analyze range pickers so Analyze and Compare can never
/// disagree about what scheme/refresh/state was in effect.
pub fn range_context(s: &SessionData, from_ms: u64, to_ms: u64) -> RangeContext {
    RangeContext {
        power_scheme: analyze::policy_last_window(s, from_ms, to_ms),
        power_source: analyze::ac_last_window(s, from_ms, to_ms),
        refresh_hz: analyze::refresh_last_window(s, from_ms, to_ms),
        display_count: s
            .display
            .iter()
            .rfind(|p| {
                let w = analyze::mono_wall(s, p.t);
                w >= from_ms && w <= to_ms
            })
            .map(|p| p.displays.len()),
        gpu_adapters: s
            .gpu
            .iter()
            .rfind(|p| {
                let w = analyze::mono_wall(s, p.t);
                w >= from_ms && w <= to_ms
            })
            .map(|p| p.adapters.iter().map(|a| a.name.clone()).collect())
            .unwrap_or_default(),
        effective_cadence_ms: effective_cadence_ms(s, from_ms, to_ms),
    }
}

fn effective_cadence_ms(s: &SessionData, from_ms: u64, to_ms: u64) -> Option<f64> {
    let ts: Vec<f64> = s
        .battery
        .iter()
        .filter(|p| {
            let w = analyze::wall_of(s, &p.discharge, p.t);
            w >= from_ms && w <= to_ms
        })
        .map(|p| p.t)
        .collect();
    if ts.len() < 2 {
        return None;
    }
    let mut d: Vec<f64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    d.sort_by(|a, b| a.total_cmp(b));
    d.get(d.len() / 2).copied().map(|x| x * 1000.0)
}

fn session_recovered(s: &SessionData) -> Option<bool> {
    let footer = s.footer.as_ref()?;
    footer
        .get("recovered")
        .and_then(|b| b.as_bool())
        .or_else(|| {
            footer
                .get("summary")
                .and_then(|sum| sum.get("recovered"))
                .and_then(|b| b.as_bool())
        })
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

    /// A session of `n` battery samples at `step_ms`, constant discharge.
    fn battery_session(w: f64, n: u64, step_ms: u64) -> SessionData {
        let mut s = SessionData {
            label: "t".to_string(),
            wall_base_ms: 1000,
            ..Default::default()
        };
        for i in 0..n {
            s.battery.push(BatteryPoint {
                t: (i * step_ms) as f64 / 1000.0,
                discharge: Telemetry::measured(w, "battery", stamp(1000 + i * step_ms)),
                ..Default::default()
            });
        }
        s
    }

    /// Same shape but with a repeating set of distinct readings, so the
    /// effective sample count is not collapsed by a cached sensor.
    fn battery_varying(base: f64, n: u64, step_ms: u64) -> SessionData {
        let mut s = SessionData {
            label: "v".to_string(),
            wall_base_ms: 1000,
            ..Default::default()
        };
        for i in 0..n {
            let v = base + (i % 7) as f64 * 0.4;
            s.battery.push(BatteryPoint {
                t: (i * step_ms) as f64 / 1000.0,
                discharge: Telemetry::measured(v, "battery", stamp(1000 + i * step_ms)),
                ..Default::default()
            });
        }
        s
    }

    #[test]
    fn identical_ranges_have_zero_delta() {
        let s = battery_session(6.0, 60, 1000);
        let out = compare_ranges(
            ComparisonInput::whole(&s, 1000),
            ComparisonInput::whole(&s, 1000),
        );
        let power = out
            .headline_metrics
            .iter()
            .find(|m| m.metric == "battery_discharge_w")
            .unwrap();
        assert_eq!(power.absolute_delta, Some(0.0));
        assert_eq!(power.direction, "unchanged");
        assert_eq!(power.comparability, ComparabilityLevel::Compatible);
        assert!(out.ranked_changes.iter().all(|r| r.delta == 0.0));
    }

    #[test]
    fn unequal_duration_prefers_normalized_energy() {
        let short = battery_session(6.0, 60, 1000);
        let long = battery_session(6.0, 600, 1000);
        let out = compare_ranges(
            ComparisonInput::whole(&short, 1000),
            ComparisonInput::whole(&long, 1000),
        );
        assert!(!out.energy.durations_similar);
        assert_eq!(out.energy.preferred_basis, "average_power");
        // Same average power even though totals differ 10x.
        let a = out.energy.avg_power_w_a.unwrap();
        let b = out.energy.avg_power_w_b.unwrap();
        assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        assert!(out.energy.total_wh_b.unwrap() > out.energy.total_wh_a.unwrap() * 5.0);
        assert!(out.energy.normalized_wh_a.is_some());
    }

    #[test]
    fn missing_metric_on_one_side_is_not_comparable() {
        let mut a = battery_session(6.0, 60, 1000);
        for i in 0..60 {
            a.cpu.push(CpuPoint {
                t: i as f64,
                utility: Telemetry::measured(10.0, "cpu", stamp(1000 + i * 1000)),
                ..Default::default()
            });
        }
        let b = battery_session(5.0, 60, 1000);
        let out = compare_ranges(
            ComparisonInput::whole(&a, 1000),
            ComparisonInput::whole(&b, 1000),
        );
        let cpu = out
            .headline_metrics
            .iter()
            .find(|m| m.metric == "cpu_utility_pct")
            .unwrap();
        assert_eq!(cpu.comparability, ComparabilityLevel::NotComparable);
        assert!(cpu.comparability_reason.contains("missing evidence in B"));
        assert_eq!(cpu.direction, "missing_b");
    }

    #[test]
    fn low_coverage_produces_a_caveat_and_weak_level() {
        let mut s = battery_session(6.0, 60, 1000);
        // Punch a large hole but keep both ends, so the span stays ~59 s while
        // most of it becomes unobserved.
        s.battery.retain(|p| p.t < 5.0 || p.t >= 55.0);
        let out = compare_ranges(
            ComparisonInput::whole(&s, 1000),
            ComparisonInput::whole(&s, 1000),
        );
        assert!(out.a.coverage.unwrap() < WEAK_COVERAGE);
        assert!(
            out.caveats
                .iter()
                .any(|c| c.scope == "coverage" && c.severity == "critical"),
            "{:?}",
            out.caveats
        );
        assert!(out.comparability.overall == ComparabilityLevel::Weak);
    }

    #[test]
    fn charging_vs_discharging_is_not_comparable() {
        let discharge = {
            let mut s = battery_session(6.0, 60, 1000);
            for p in s.battery.iter_mut() {
                p.charge = Telemetry::unavailable(
                    crate::telemetry::UnavailKind::NotSampled,
                    "not charging",
                    "battery",
                    stamp(1000),
                );
            }
            s
        };
        let mut charge = battery_session(0.0, 60, 1000);
        for p in charge.battery.iter_mut() {
            p.discharge = Telemetry::unavailable(
                crate::telemetry::UnavailKind::NotSampled,
                "on ac",
                "battery",
                stamp(1000),
            );
            p.charge = Telemetry::measured(20.0, "battery", stamp(1000));
        }
        let out = compare_ranges(
            ComparisonInput::whole(&discharge, 1000),
            ComparisonInput::whole(&charge, 1000),
        );
        let power = out
            .headline_metrics
            .iter()
            .find(|m| m.metric == "battery_discharge_w")
            .unwrap();
        assert_eq!(power.comparability, ComparabilityLevel::NotComparable);
        assert!(power.comparability_reason.contains("charging"));
    }

    #[test]
    fn different_cadence_is_a_caveat() {
        let fast = battery_session(6.0, 400, 250);
        let slow = battery_session(6.0, 100, 1000);
        let out = compare_ranges(
            ComparisonInput::whole(&fast, 250),
            ComparisonInput::whole(&slow, 1000),
        );
        assert!(
            out.comparability
                .findings
                .iter()
                .any(|f| f.reason.contains("cadence differs")),
            "{:?}",
            out.comparability.findings
        );
    }

    #[test]
    fn process_only_in_one_side_and_incompleteness() {
        let mut a = battery_session(6.0, 60, 1000);
        a.procs.push(ProcPoint {
            t: 0.0,
            inaccessible: Some(4),
            truncated: Some(true),
            top: vec![
                ProcEntry {
                    pid: 1,
                    name: "OnlyA.exe".to_string(),
                    cpu: Telemetry::measured(40.0, "proc", stamp(1000)),
                    ..Default::default()
                },
                ProcEntry {
                    pid: 9,
                    name: "shared.exe".to_string(),
                    cpu: Telemetry::measured(10.0, "proc", stamp(1000)),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let mut b = battery_session(6.0, 60, 1000);
        b.procs.push(ProcPoint {
            t: 0.0,
            inaccessible: Some(0),
            truncated: Some(false),
            top: vec![ProcEntry {
                pid: 7,
                name: "OnlyB.EXE".to_string(),
                cpu: Telemetry::measured(30.0, "proc", stamp(1000)),
                ..Default::default()
            }],
            ..Default::default()
        });
        let out = compare_ranges(
            ComparisonInput::whole(&a, 1000),
            ComparisonInput::whole(&b, 1000),
        );
        assert!(out.processes.incomplete);
        let only_a = out
            .processes
            .rows
            .iter()
            .find(|r| r.identity == "onlya")
            .unwrap();
        assert_eq!(only_a.presence, "a_only");
        assert!(only_a.note.contains("not observed in B"));
        let only_b = out
            .processes
            .rows
            .iter()
            .find(|r| r.identity == "onlyb")
            .unwrap();
        assert_eq!(only_b.presence, "b_only");
        assert!(
            out.caveats.iter().any(|c| c.scope == "processes"),
            "{:?}",
            out.caveats
        );
    }

    #[test]
    fn no_percent_against_a_zero_denominator() {
        // CPU utility is unsuitable for percentages; a zero base must not emit
        // one either.
        let a = {
            let mut s = battery_session(6.0, 60, 1000);
            for p in s.battery.iter_mut() {
                p.discharge = Telemetry::measured(0.0, "battery", stamp(1000));
            }
            s
        };
        let b = battery_session(3.0, 60, 1000);
        let out = compare_ranges(
            ComparisonInput::whole(&a, 1000),
            ComparisonInput::whole(&b, 1000),
        );
        let power = out
            .headline_metrics
            .iter()
            .find(|m| m.metric == "battery_discharge_w")
            .unwrap();
        assert!(power.relative_delta.is_none(), "{power:?}");
        assert_eq!(power.absolute_delta, Some(3.0));
    }

    #[test]
    fn correlation_reports_insufficient() {
        let s = battery_session(6.0, 60, 1000);
        let out = compare_ranges(
            ComparisonInput::whole(&s, 1000),
            ComparisonInput::whole(&s, 1000),
        );
        assert!(
            out.correlations.iter().all(|c| !c.comparable),
            "{:?}",
            out.correlations
        );
    }

    #[test]
    fn reliability_gates_noise() {
        let a = battery_varying(6.0, 60, 1000);
        let b = battery_varying(6.2, 60, 1000);
        let out = compare_ranges(
            ComparisonInput::whole(&a, 1000),
            ComparisonInput::whole(&b, 1000),
        );
        let power = out
            .headline_metrics
            .iter()
            .find(|m| m.metric == "battery_discharge_w")
            .unwrap();
        // 0.2 W delta < 0.5 W threshold: no percent, difference within noise.
        assert_eq!(power.direction, "unchanged");
        assert_eq!(power.reliability.level, "within_noise", "{power:?}");
        // A relative change is withheld for a within-noise difference only by
        // the threshold; the effect size remains explicitly available.
        assert!(power.reliability.effect_size.is_some());
        assert!(power.reliability.effect_size.unwrap() < 0.5);
    }

    #[test]
    fn a_constant_sensor_yields_one_effective_sample() {
        // Repeated identical readings are not independent observations.
        let a = battery_session(6.0, 60, 1000);
        let b = battery_varying(6.0, 60, 1000);
        let out = compare_ranges(
            ComparisonInput::whole(&a, 1000),
            ComparisonInput::whole(&b, 1000),
        );
        let power = out
            .headline_metrics
            .iter()
            .find(|m| m.metric == "battery_discharge_w")
            .unwrap();
        assert_eq!(power.reliability.level, "insufficient", "{power:?}");
    }

    #[test]
    fn whole_session_vs_selected_range_share_one_abstraction() {
        let s = battery_session(6.0, 120, 1000);
        let out = compare_ranges(
            ComparisonInput::whole(&s, 1000),
            ComparisonInput::range(&s, 1000, 1000 + 30_000, 1000 + 60_000),
        );
        assert!(out.a.whole_session);
        assert!(!out.b.whole_session);
        assert!(out.a.duration_s > out.b.duration_s);
    }

    #[test]
    fn relative_alignment_inputs_resolve_without_forking() {
        let s = battery_session(6.0, 60, 1000);
        // One-sided inputs extend to the span edge rather than inventing a bound.
        let (from, to) = ComparisonInput {
            session: &s,
            interval_ms: 1000,
            from_ms: Some(1000 + 10_000),
            to_ms: None,
        }
        .resolved_range();
        assert_eq!(from, 1000 + 10_000);
        assert!(to > from);
    }
}
