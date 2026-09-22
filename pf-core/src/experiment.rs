//! Run-level experiment analysis: did a change actually make a measurable
//! difference?
//!
//! This module sits on top of [`crate::compare::ComparisonInput`] and the
//! range analysis. Each run is reduced to **one run-level outcome** first
//! (a median plus its own quality), and the between-group statistics then
//! operate over those run-level outcomes. The experimental unit is the RUN —
//! thousands of telemetry ticks inside one run are never treated as thousands
//! of independent repetitions. This avoids pseudoreplication and keeps the
//! conservative effective-N discipline from the Compare engine.
//!
//! Everything here is pure, deterministic and rule-based: no ML, no LLM, and
//! no causal claim. A large difference in a confounded experiment is still
//! reported as confounded rather than as a strong conclusion.

use crate::analyze::{self, RangeAnalysis};
use crate::compare::{self, ComparisonInput, RangeContext};
use crate::stats;

/// Bump when the aggregation or wording changes so cached results recompute.
pub const EXPERIMENT_ANALYSIS_VERSION: u32 = 1;

/// Default number of runs per group before a result is more than exploratory.
pub const MIN_USEFUL_RUNS_PER_GROUP: usize = 2;

const SHORT_RUN_S: f64 = 5.0;
const WEAK_COVERAGE: f64 = 0.5;
const LOW_COVERAGE: f64 = 0.7;
const CADENCE_RATIO: f64 = 3.0;
const DURATION_RATIO: f64 = 1.5;
const COVERAGE_DISPARITY: f64 = 0.2;
const BOOTSTRAP_ITERS: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunGroup {
    Baseline,
    Treatment,
}

impl RunGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            RunGroup::Baseline => "baseline",
            RunGroup::Treatment => "treatment",
        }
    }
}

/// Wording only: it never changes the statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreatmentDirection {
    Lower,
    Higher,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pairing {
    Unpaired,
    Paired,
}

impl Pairing {
    pub fn as_str(self) -> &'static str {
        match self {
            Pairing::Paired => "paired",
            Pairing::Unpaired => "unpaired",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExperimentConfig {
    pub primary_metric: String,
    pub primary_label: String,
    pub primary_unit: String,
    pub direction: TreatmentDirection,
    pub pairing: Pairing,
}

#[derive(Debug, Clone)]
pub struct RunMeta {
    pub id: String,
    pub label: String,
    pub group: RunGroup,
    pub order: usize,
    /// Identifies a baseline/treatment pair when the design is paired.
    pub pair_id: Option<usize>,
    pub included: bool,
}

/// One experimental run: metadata plus the same range input Compare uses.
pub struct RunInput<'a> {
    pub meta: RunMeta,
    pub input: ComparisonInput<'a>,
}

/// Per-run validation. Bad runs are reported, never silently deleted.
#[derive(Debug, Clone, PartialEq)]
pub struct RunValidation {
    /// "accepted" | "accepted_with_caveats" | "invalid"
    pub status: &'static str,
    pub primary_present: bool,
    pub findings: Vec<String>,
}

impl RunValidation {
    pub fn valid(&self) -> bool {
        self.status != "invalid"
    }
}

/// One run reduced to a single run-level outcome (the experimental unit).
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    pub id: String,
    pub label: String,
    pub group: RunGroup,
    pub order: usize,
    pub pair_id: Option<usize>,
    pub included: bool,
    pub from_ms: u64,
    pub to_ms: u64,
    pub duration_s: f64,
    pub primary_value: Option<f64>,
    pub primary_known: usize,
    pub coverage: Option<f64>,
    pub discontinuities: usize,
    pub timeouts: usize,
    pub cadence_ms: Option<f64>,
    pub context: RangeContext,
    pub collectors: Vec<String>,
    pub process_incomplete: Option<bool>,
    pub process_names: Vec<String>,
    /// (key, label, unit, run median) for every curated secondary metric.
    pub secondary: Vec<(String, String, String, Option<f64>)>,
    pub validation: RunValidation,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupSummary {
    pub group: RunGroup,
    pub runs_total: usize,
    pub runs_included: usize,
    pub runs_excluded: usize,
    pub runs_with_primary: usize,
    pub runs_valid: usize,
    pub runs_invalid: usize,
    pub median: Option<f64>,
    pub mean: Option<f64>,
    /// Robust run-to-run spread (median absolute deviation).
    pub spread: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub coverage_median: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PairedDifference {
    pub pair_id: usize,
    pub baseline_run: String,
    pub treatment_run: String,
    pub baseline: f64,
    pub treatment: f64,
    pub delta: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PairedReport {
    pub pairs: Vec<PairedDifference>,
    pub usable_pairs: usize,
    pub median_delta: Option<f64>,
    pub spread: Option<f64>,
    /// Every usable pair moved in the same direction.
    pub consistent: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoiseAssessment {
    /// "none" | "run_to_run" | "insufficient"
    pub source: &'static str,
    pub estimate: Option<f64>,
    pub note: String,
}

/// Conservative evidence classification. Separate from experiment validity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExperimentResult {
    ConsistentDifference,
    PossibleDifference,
    WithinNoise,
    NoDifferenceDetected,
    Insufficient,
    Confounded,
}

impl ExperimentResult {
    pub fn as_str(self) -> &'static str {
        match self {
            ExperimentResult::ConsistentDifference => "consistent_difference",
            ExperimentResult::PossibleDifference => "possible_difference",
            ExperimentResult::WithinNoise => "within_noise",
            ExperimentResult::NoDifferenceDetected => "no_difference_detected",
            ExperimentResult::Insufficient => "insufficient",
            ExperimentResult::Confounded => "confounded",
        }
    }
}

/// Experiment validity is reported independently of whether an effect was
/// found: a large delta in a confounded experiment is still confounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExperimentValidity {
    Valid,
    ValidWithCaveats,
    Weak,
    Confounded,
    Insufficient,
}

impl ExperimentValidity {
    pub fn as_str(self) -> &'static str {
        match self {
            ExperimentValidity::Valid => "valid",
            ExperimentValidity::ValidWithCaveats => "valid_with_caveats",
            ExperimentValidity::Weak => "weak",
            ExperimentValidity::Confounded => "confounded",
            ExperimentValidity::Insufficient => "insufficient",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectEstimate {
    pub baseline_value: Option<f64>,
    pub treatment_value: Option<f64>,
    pub absolute_delta: Option<f64>,
    /// Only when the baseline denominator is non-zero.
    pub relative_delta: Option<f64>,
    pub direction: &'static str,
    pub effect_size: Option<f64>,
    /// Deterministic bootstrap 5–95% interval over run-level outcomes.
    pub confidence_interval: Option<(f64, f64)>,
    pub paired: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Confounder {
    pub key: &'static str,
    pub label: &'static str,
    /// "controlled" | "changed" | "unknown"
    pub state: &'static str,
    pub evidence: String,
    /// Runs whose evidence differs from the majority (may be empty).
    pub runs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SecondaryComparison {
    pub metric: String,
    pub label: String,
    pub unit: String,
    pub baseline_median: Option<f64>,
    pub treatment_median: Option<f64>,
    pub delta: Option<f64>,
    pub comparable: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentAnalysis {
    pub analysis_version: u32,
    pub primary_metric: String,
    pub primary_label: String,
    pub primary_unit: String,
    pub direction: TreatmentDirection,
    pub pairing: Pairing,
    pub runs: Vec<RunOutcome>,
    pub baseline: GroupSummary,
    pub treatment: GroupSummary,
    pub effect: EffectEstimate,
    pub paired: Option<PairedReport>,
    pub noise: NoiseAssessment,
    pub classification: ExperimentResult,
    pub validity: ExperimentValidity,
    pub validity_reasons: Vec<String>,
    /// Rust-authored, observational wording. The UI renders these verbatim.
    pub summary_lines: Vec<String>,
    pub confounders: Vec<Confounder>,
    pub secondary: Vec<SecondaryComparison>,
    pub caveats: Vec<String>,
}

// ---------------------------------------------------------------------------
// Top-level analysis
// ---------------------------------------------------------------------------

/// Reduce every run to a run-level outcome, then compare the groups over those
/// outcomes. `runs` may contain excluded runs: they stay visible but are held
/// out of the group statistics.
pub fn analyze_experiment(runs: &[RunInput<'_>], config: &ExperimentConfig) -> ExperimentAnalysis {
    let outcomes: Vec<RunOutcome> = runs.iter().map(|r| reduce_run(r, config)).collect();
    analyze_outcomes(outcomes, config)
}

/// Aggregate already-reduced run outcomes. Exposed separately so a caller can
/// cache per-run reductions (keyed by session fingerprint and range) and
/// recompute only the between-run statistics when inclusion changes.
pub fn analyze_outcomes(
    outcomes: Vec<RunOutcome>,
    config: &ExperimentConfig,
) -> ExperimentAnalysis {
    let baseline = summarize(RunGroup::Baseline, &outcomes);
    let treatment = summarize(RunGroup::Treatment, &outcomes);
    let secondary = secondary_comparison(&outcomes, &config.primary_metric);
    let confounders = confounders(&outcomes);
    let paired = (config.pairing == Pairing::Paired).then(|| paired_report(&outcomes));

    let critical = confounders
        .iter()
        .any(|c| c.state == "changed" && matches!(c.key, "power_source" | "battery_state"));
    let invalid_runs = outcomes
        .iter()
        .filter(|o| o.included && !o.validation.valid())
        .count();
    let missing_primary = outcomes
        .iter()
        .filter(|o| o.included && o.primary_value.is_none())
        .count();

    let baseline_values = group_values(&outcomes, RunGroup::Baseline);
    let treatment_values = group_values(&outcomes, RunGroup::Treatment);
    let noise = noise_assessment(&baseline, &treatment, paired.as_ref());
    let effect = effect_estimate(
        &baseline,
        &treatment,
        &noise,
        paired.as_ref(),
        config,
        &baseline_values,
        &treatment_values,
    );
    let (classification, validity, mut validity_reasons) = classify(
        &baseline,
        &treatment,
        &effect,
        &noise,
        paired.as_ref(),
        critical,
        invalid_runs,
        missing_primary,
    );

    let mut caveats = Vec::new();
    for c in &confounders {
        if c.state != "controlled" {
            caveats.push(format!("{}: {} ({})", c.label, c.state, c.evidence));
        }
    }
    if invalid_runs > 0 {
        caveats.push(format!(
            "{invalid_runs} included run(s) failed quality validation and may bias the result."
        ));
    }
    if missing_primary > 0 {
        caveats.push(format!(
            "{missing_primary} included run(s) have no {}. The primary metric was not switched.",
            config.primary_label
        ));
    }
    if let Some(p) = &paired
        && p.usable_pairs < p.pairs.len()
    {
        caveats.push(format!(
            "Only {} of {} paired run(s) have both sides; the rest are reported but not paired.",
            p.usable_pairs,
            p.pairs.len()
        ));
    }

    let summary_lines = summary_lines(
        config,
        &baseline,
        &treatment,
        &effect,
        &noise,
        paired.as_ref(),
        classification,
    );
    validity_reasons.shrink_to_fit();

    ExperimentAnalysis {
        analysis_version: EXPERIMENT_ANALYSIS_VERSION,
        primary_metric: config.primary_metric.clone(),
        primary_label: config.primary_label.clone(),
        primary_unit: config.primary_unit.clone(),
        direction: config.direction,
        pairing: config.pairing,
        runs: outcomes,
        baseline,
        treatment,
        effect,
        paired,
        noise,
        classification,
        validity,
        validity_reasons,
        summary_lines,
        confounders,
        secondary,
        caveats,
    }
}

// ---------------------------------------------------------------------------
// Run reduction
// ---------------------------------------------------------------------------

/// Reduce one run to a single run-level outcome (the experimental unit).
/// Pure: the caller owns the session and the range.
pub fn reduce_run(run: &RunInput<'_>, config: &ExperimentConfig) -> RunOutcome {
    let curated = compare::curated_metric_keys();
    let curated = curated.as_slice();
    let (from_ms, to_ms) = run.input.resolved_range();
    let analysis: RangeAnalysis =
        analyze::analyze_range(run.input.session, run.input.interval_ms, from_ms, to_ms);
    let context = compare::range_context(run.input.session, from_ms, to_ms);
    let duration_s = (to_ms.saturating_sub(from_ms)) as f64 / 1000.0;
    let primary = find_median(&analysis, &config.primary_metric);
    let primary_known = find_known(&analysis, &config.primary_metric);
    let secondary = curated
        .iter()
        .filter(|k| **k != config.primary_metric.as_str())
        .map(|k| {
            let stat = find_metric_stat(&analysis, k);
            (
                (*k).to_string(),
                stat.map(|m| m.label.to_string()).unwrap_or_default(),
                stat.map(|m| m.unit.to_string()).unwrap_or_default(),
                stat.and_then(|m| m.stat.median),
            )
        })
        .collect();
    let collectors: Vec<String> = analysis
        .quality
        .collector_counts
        .iter()
        .map(|(n, _)| (*n).to_string())
        .collect();
    let process_names: Vec<String> = analysis
        .processes
        .entries
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let cadence_ms = context.effective_cadence_ms;

    let validation = validate_run(
        primary.is_some(),
        primary_known,
        analysis.quality.coverage,
        duration_s,
        analysis.quality.discontinuities,
        analysis.quality.timeouts,
    );

    RunOutcome {
        id: run.meta.id.clone(),
        label: run.meta.label.clone(),
        group: run.meta.group,
        order: run.meta.order,
        pair_id: run.meta.pair_id,
        included: run.meta.included,
        from_ms,
        to_ms,
        duration_s,
        primary_value: primary,
        primary_known,
        coverage: analysis.quality.coverage,
        discontinuities: analysis.quality.discontinuities,
        timeouts: analysis.quality.timeouts,
        cadence_ms,
        context,
        collectors,
        process_incomplete: analysis.processes.incomplete,
        process_names,
        secondary,
        validation,
    }
}

fn find_metric_stat<'a>(a: &'a RangeAnalysis, key: &str) -> Option<&'a analyze::MetricStat> {
    a.domains
        .iter()
        .flat_map(|d| d.metrics.iter())
        .find(|m| m.key == key)
}

fn find_median(a: &RangeAnalysis, key: &str) -> Option<f64> {
    find_metric_stat(a, key).and_then(|m| m.stat.median)
}

fn find_known(a: &RangeAnalysis, key: &str) -> usize {
    find_metric_stat(a, key).map(|m| m.stat.known).unwrap_or(0)
}

pub fn validate_run(
    primary_present: bool,
    primary_known: usize,
    coverage: Option<f64>,
    duration_s: f64,
    discontinuities: usize,
    timeouts: usize,
) -> RunValidation {
    let mut findings = Vec::new();
    if !primary_present || primary_known == 0 {
        findings.push("primary metric has no known samples".to_string());
    }
    match coverage {
        Some(c) if c < WEAK_COVERAGE => {
            findings.push(format!("low coverage {:.0}%", c * 100.0));
        }
        Some(c) if c < LOW_COVERAGE => {
            findings.push(format!("partial coverage {:.0}%", c * 100.0));
        }
        None => findings.push("coverage could not be established".to_string()),
        _ => {}
    }
    if duration_s < SHORT_RUN_S {
        findings.push(format!("very short run ({duration_s:.0}s)"));
    }
    if discontinuities > 0 {
        findings.push(format!("{discontinuities} clock discontinuity(ies)"));
    }
    if timeouts > 0 {
        findings.push(format!("{timeouts} collector timeout(s)"));
    }
    let severe = !primary_present
        || primary_known == 0
        || coverage.is_none_or(|c| c < WEAK_COVERAGE)
        || duration_s < SHORT_RUN_S;
    let status = if severe {
        "invalid"
    } else if findings.is_empty() {
        "accepted"
    } else {
        "accepted_with_caveats"
    };
    RunValidation {
        status,
        primary_present: primary_present && primary_known > 0,
        findings,
    }
}

// ---------------------------------------------------------------------------
// Group summaries
// ---------------------------------------------------------------------------

fn summarize(group: RunGroup, runs: &[RunOutcome]) -> GroupSummary {
    let group_runs: Vec<&RunOutcome> = runs.iter().filter(|r| r.group == group).collect();
    let included: Vec<&RunOutcome> = group_runs.iter().copied().filter(|r| r.included).collect();
    let values: Vec<f64> = included.iter().filter_map(|r| r.primary_value).collect();
    let deviations: Vec<f64> = values
        .iter()
        .map(|v| (v - stats::median(&values).unwrap_or(*v)).abs())
        .collect();
    let coverage_median = stats::median(
        &included
            .iter()
            .filter_map(|r| r.coverage)
            .collect::<Vec<_>>(),
    );
    GroupSummary {
        group,
        runs_total: group_runs.len(),
        runs_included: included.len(),
        runs_excluded: group_runs.len() - included.len(),
        runs_with_primary: values.len(),
        runs_valid: included.iter().filter(|r| r.validation.valid()).count(),
        runs_invalid: included.iter().filter(|r| !r.validation.valid()).count(),
        median: stats::median(&values),
        mean: stats::mean(&values),
        spread: stats::median(&deviations),
        min: values.iter().copied().reduce(f64::min),
        max: values.iter().copied().reduce(f64::max),
        coverage_median,
    }
}

fn group_values(runs: &[RunOutcome], group: RunGroup) -> Vec<f64> {
    runs.iter()
        .filter(|r| r.group == group && r.included)
        .filter_map(|r| r.primary_value)
        .collect()
}

fn paired_report(runs: &[RunOutcome]) -> PairedReport {
    let included: Vec<&RunOutcome> = runs
        .iter()
        .filter(|r| r.included && r.primary_value.is_some())
        .collect();
    let mut pair_ids: Vec<usize> = included.iter().filter_map(|r| r.pair_id).collect();
    pair_ids.sort_unstable();
    pair_ids.dedup();
    let mut pairs = Vec::new();
    for id in pair_ids {
        let b = included.iter().find(|r| {
            r.pair_id == Some(id) && r.group == RunGroup::Baseline && r.primary_value.is_some()
        });
        let t = included.iter().find(|r| {
            r.pair_id == Some(id) && r.group == RunGroup::Treatment && r.primary_value.is_some()
        });
        if let (Some(b), Some(t)) = (b, t) {
            let (bv, tv) = (b.primary_value.unwrap(), t.primary_value.unwrap());
            pairs.push(PairedDifference {
                pair_id: id,
                baseline_run: b.id.clone(),
                treatment_run: t.id.clone(),
                baseline: bv,
                treatment: tv,
                delta: tv - bv,
            });
        }
    }
    let deltas: Vec<f64> = pairs.iter().map(|p| p.delta).collect();
    let median = stats::median(&deltas);
    let consistent = deltas.len() >= 2
        && median.is_some_and(|m| deltas.iter().all(|d| d.signum() == m.signum()));
    let deviations: Vec<f64> = deltas
        .iter()
        .map(|d| (d - median.unwrap_or(*d)).abs())
        .collect();
    PairedReport {
        usable_pairs: pairs.len(),
        median_delta: median,
        spread: stats::median(&deviations),
        consistent,
        pairs,
    }
}

// ---------------------------------------------------------------------------
// Noise, effect, classification
// ---------------------------------------------------------------------------

fn noise_assessment(
    baseline: &GroupSummary,
    treatment: &GroupSummary,
    paired: Option<&PairedReport>,
) -> NoiseAssessment {
    let paired_usable = paired.filter(|p| p.usable_pairs >= 2);
    let estimate = if let Some(p) = paired_usable {
        p.spread.or(baseline.spread).or(treatment.spread)
    } else {
        match (baseline.spread, treatment.spread) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    };
    let source = if paired.is_some() && paired_usable.is_none() {
        "insufficient"
    } else if estimate.is_some() {
        "run_to_run"
    } else {
        "none"
    };
    let note = match (source, estimate) {
        ("run_to_run", Some(e)) => format!(
            "Run-to-run spread is about {e:.3} (median absolute deviation of {}).",
            if paired.is_some() {
                "paired differences"
            } else {
                "run-level group outcomes"
            }
        ),
        ("none", _) => {
            "No run-to-run variation was observed, so no noise floor can be estimated.".to_string()
        }
        _ => "Not enough runs to estimate run-to-run variation.".to_string(),
    };
    NoiseAssessment {
        source,
        estimate,
        note,
    }
}

#[allow(clippy::too_many_arguments)]
fn effect_estimate(
    baseline: &GroupSummary,
    treatment: &GroupSummary,
    noise: &NoiseAssessment,
    paired: Option<&PairedReport>,
    config: &ExperimentConfig,
    baseline_values: &[f64],
    treatment_values: &[f64],
) -> EffectEstimate {
    let (b, t) = (baseline.median, treatment.median);
    let delta = match (b, t) {
        (Some(b), Some(t)) => Some(t - b),
        _ => None,
    };
    // Never a percentage against a zero or near-zero denominator.
    let relative = delta.and_then(|d| match b {
        Some(base) if base.abs() > 1e-9 => Some(d / base),
        _ => None,
    });
    let direction = match (delta, config.direction) {
        (Some(d), TreatmentDirection::Lower) if d < 0.0 => "lower",
        (Some(d), TreatmentDirection::Lower) if d > 0.0 => "higher",
        (Some(d), TreatmentDirection::Higher) if d > 0.0 => "higher",
        (Some(d), TreatmentDirection::Higher) if d < 0.0 => "lower",
        (Some(d), _) if d > 0.0 => "higher",
        (Some(d), _) if d < 0.0 => "lower",
        _ => "unchanged",
    };
    let effect_size = match (delta, noise.estimate) {
        (Some(d), Some(n)) if n > 0.0 => Some(d.abs() / n),
        _ => None,
    };
    let confidence_interval = bootstrap_ci(baseline_values, treatment_values, paired);
    EffectEstimate {
        baseline_value: b,
        treatment_value: t,
        absolute_delta: delta,
        relative_delta: relative,
        direction,
        effect_size,
        confidence_interval,
        paired: paired.is_some_and(|p| p.usable_pairs >= 2),
    }
}

/// Deterministic bootstrap over the actual run-level outcomes (not telemetry
/// samples). Returns `None` when fewer than three observations per group (or
/// three usable pairs) make an interval uninformative.
fn bootstrap_ci(
    baseline_values: &[f64],
    treatment_values: &[f64],
    paired: Option<&PairedReport>,
) -> Option<(f64, f64)> {
    if let Some(p) = paired.filter(|p| p.usable_pairs >= 3) {
        let deltas: Vec<f64> = p.pairs.iter().map(|x| x.delta).collect();
        return bootstrap_median(&[deltas]);
    }
    if baseline_values.len() < 3 || treatment_values.len() < 3 {
        return None;
    }
    bootstrap_median(&[baseline_values.to_vec(), treatment_values.to_vec()])
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        // Numerical Recipes LCG; deterministic across platforms.
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }
    fn index(&mut self, n: usize) -> usize {
        (self.next() >> 33) as usize % n.max(1)
    }
}

fn bootstrap_median(groups: &[Vec<f64>]) -> Option<(f64, f64)> {
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    let mut diffs = Vec::with_capacity(BOOTSTRAP_ITERS);
    for _ in 0..BOOTSTRAP_ITERS {
        let mut medians = Vec::with_capacity(groups.len());
        for g in groups {
            if g.is_empty() {
                return None;
            }
            let mut sample = Vec::with_capacity(g.len());
            for _ in 0..g.len() {
                sample.push(g[rng.index(g.len())]);
            }
            medians.push(stats::median(&sample)?);
        }
        // Two groups: treatment - baseline. One group: the value itself.
        let d = if medians.len() >= 2 {
            medians[medians.len() - 1] - medians[0]
        } else {
            medians[0]
        };
        diffs.push(d);
    }
    diffs.sort_by(|a, b| a.total_cmp(b));
    let lo = stats::percentile_sorted(&mut diffs.clone(), 5.0)?;
    let hi = stats::percentile_sorted(&mut diffs, 95.0)?;
    Some((lo, hi))
}

#[allow(clippy::too_many_arguments)]
fn classify(
    baseline: &GroupSummary,
    treatment: &GroupSummary,
    effect: &EffectEstimate,
    noise: &NoiseAssessment,
    paired: Option<&PairedReport>,
    critical_confounder: bool,
    invalid_runs: usize,
    missing_primary: usize,
) -> (ExperimentResult, ExperimentValidity, Vec<String>) {
    let mut reasons = Vec::new();
    let n_a = baseline.runs_with_primary;
    let n_b = treatment.runs_with_primary;

    if critical_confounder {
        reasons.push(
            "a critical confounder changed between groups (power source / battery state)"
                .to_string(),
        );
        return (
            ExperimentResult::Confounded,
            ExperimentValidity::Confounded,
            reasons,
        );
    }
    if n_a == 0 || n_b == 0 || missing_primary > 0 && (n_a == 0 || n_b == 0) {
        reasons.push("at least one group has no usable primary-metric run".to_string());
        return (
            ExperimentResult::Insufficient,
            ExperimentValidity::Insufficient,
            reasons,
        );
    }
    if n_a < MIN_USEFUL_RUNS_PER_GROUP || n_b < MIN_USEFUL_RUNS_PER_GROUP {
        reasons.push(format!(
            "exploratory only: {n_a} baseline / {n_b} treatment run(s) with the primary metric"
        ));
        return (
            ExperimentResult::Insufficient,
            ExperimentValidity::Insufficient,
            reasons,
        );
    }
    if missing_primary > 0 {
        reasons.push(format!(
            "{missing_primary} included run(s) lack the primary metric"
        ));
    }
    if invalid_runs > 0 {
        reasons.push(format!(
            "{invalid_runs} included run(s) failed quality validation"
        ));
    }

    let Some(delta) = effect.absolute_delta else {
        reasons.push("no group difference could be computed".to_string());
        return (
            ExperimentResult::Insufficient,
            ExperimentValidity::Insufficient,
            reasons,
        );
    };

    let (result, validity) = if delta == 0.0 {
        reasons.push("the group outcomes are identical".to_string());
        (
            ExperimentResult::NoDifferenceDetected,
            ExperimentValidity::Valid,
        )
    } else {
        match noise.estimate {
            None => {
                reasons.push("too few runs to estimate run-to-run noise".to_string());
                (
                    ExperimentResult::PossibleDifference,
                    ExperimentValidity::Weak,
                )
            }
            Some(noise) => {
                let ratio = if noise > 0.0 {
                    delta.abs() / noise
                } else {
                    f64::INFINITY
                };
                let consistent = paired.is_some_and(|p| p.consistent && p.usable_pairs >= 2);
                if noise == 0.0 {
                    reasons.push("no observed run-to-run variation".to_string());
                    (
                        ExperimentResult::ConsistentDifference,
                        ExperimentValidity::Weak,
                    )
                } else if delta.abs() <= noise {
                    reasons.push(format!(
                        "the {:.3} difference is within the observed run-to-run spread {noise:.3}",
                        delta
                    ));
                    (ExperimentResult::WithinNoise, ExperimentValidity::Weak)
                } else if ratio >= 2.0 && (consistent || n_a.min(n_b) >= 3) {
                    reasons.push(format!(
                        "difference {:.3} exceeds run-to-run spread {noise:.3} ({ratio:.1}x)",
                        delta
                    ));
                    (
                        ExperimentResult::ConsistentDifference,
                        ExperimentValidity::ValidWithCaveats,
                    )
                } else {
                    reasons.push(format!(
                        "difference {:.3} is larger than the spread {noise:.3} but not decisive",
                        delta
                    ));
                    (
                        ExperimentResult::PossibleDifference,
                        ExperimentValidity::Weak,
                    )
                }
            }
        }
    };

    let mut validity = validity;
    if invalid_runs > 0 || missing_primary > 0 {
        validity = match validity {
            ExperimentValidity::Valid => ExperimentValidity::ValidWithCaveats,
            other => other,
        };
    }
    (result, validity, reasons)
}

// ---------------------------------------------------------------------------
// Confounders
// ---------------------------------------------------------------------------

fn confounders(runs: &[RunOutcome]) -> Vec<Confounder> {
    let included: Vec<&RunOutcome> = runs.iter().filter(|r| r.included).collect();
    let mut out = Vec::new();

    let mut push = |key: &'static str,
                    label: &'static str,
                    state: &'static str,
                    evidence: String,
                    runs: Vec<String>| {
        out.push(Confounder {
            key,
            label,
            state,
            evidence,
            runs,
        });
    };

    let run_ids: Vec<String> = included.iter().map(|r| r.id.clone()).collect();

    // Power source / battery state.
    push_variation(
        "power_source",
        "Power source",
        &included
            .iter()
            .map(|r| r.context.power_source.clone())
            .collect::<Vec<_>>(),
        &run_ids,
        &mut push,
    );

    // Power scheme.
    push_variation(
        "power_scheme",
        "Power scheme",
        &included
            .iter()
            .map(|r| r.context.power_scheme.clone())
            .collect::<Vec<_>>(),
        &run_ids,
        &mut push,
    );

    // Display refresh (a likely treatment variable, so reported not judged).
    push_variation(
        "display_refresh",
        "Display refresh",
        &included
            .iter()
            .map(|r| r.context.refresh_hz.clone())
            .collect::<Vec<_>>(),
        &run_ids,
        &mut push,
    );

    // Effective cadence.
    let cadences: Vec<Option<f64>> = included.iter().map(|r| r.cadence_ms).collect();
    let cad_values: Vec<f64> = cadences.iter().flatten().copied().collect();
    let cadence_state = if cad_values.len() < included.len() {
        "unknown"
    } else if let (Some(lo), Some(hi)) = (
        cad_values.iter().copied().reduce(f64::min),
        cad_values.iter().copied().reduce(f64::max),
    ) {
        if lo > 0.0 && hi / lo > CADENCE_RATIO {
            "changed"
        } else {
            "controlled"
        }
    } else {
        "unknown"
    };
    push(
        "cadence",
        "Effective cadence",
        cadence_state,
        if cadence_state == "controlled" {
            "sample cadence is comparable across runs".to_string()
        } else {
            format!(
                "cadence ranges from {} to {} ms",
                cad_values
                    .iter()
                    .copied()
                    .reduce(f64::min)
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_else(|| "?".into()),
                cad_values
                    .iter()
                    .copied()
                    .reduce(f64::max)
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_else(|| "?".into())
            )
        },
        Vec::new(),
    );

    // Run duration.
    let durations: Vec<f64> = included.iter().map(|r| r.duration_s).collect();
    let duration_state = match (
        durations.iter().copied().reduce(f64::min),
        durations.iter().copied().reduce(f64::max),
    ) {
        (Some(lo), Some(hi)) if lo > 0.0 && hi / lo > DURATION_RATIO => "changed",
        (Some(_), Some(_)) => "controlled",
        _ => "unknown",
    };
    push(
        "run_duration",
        "Run duration",
        duration_state,
        if duration_state == "controlled" {
            "run durations are comparable".to_string()
        } else {
            "run durations differ substantially".to_string()
        },
        Vec::new(),
    );

    // Coverage disparity.
    let coverage_state = {
        let known: Vec<f64> = included.iter().filter_map(|r| r.coverage).collect();
        if known.len() < included.len() {
            "unknown"
        } else {
            let lo = known.iter().copied().reduce(f64::min).unwrap_or(0.0);
            let hi = known.iter().copied().reduce(f64::max).unwrap_or(0.0);
            if hi - lo > COVERAGE_DISPARITY {
                "changed"
            } else {
                "controlled"
            }
        }
    };
    push(
        "coverage",
        "Coverage",
        coverage_state,
        "coverage disparity between runs".to_string(),
        Vec::new(),
    );

    // Collector availability.
    let collector_state = if sets_equal(included.iter().map(|r| r.collectors.clone()).collect()) {
        "controlled"
    } else {
        "changed"
    };
    push(
        "collectors",
        "Collector availability",
        collector_state,
        "the set of collectors with samples in range differs".to_string(),
        Vec::new(),
    );

    // Clock discontinuities.
    let disc_runs: Vec<String> = included
        .iter()
        .filter(|r| r.discontinuities > 0)
        .map(|r| r.id.clone())
        .collect();
    push(
        "discontinuities",
        "Clock discontinuities",
        if disc_runs.is_empty() {
            "controlled"
        } else {
            "changed"
        },
        if disc_runs.is_empty() {
            "no clock discontinuity recorded".to_string()
        } else {
            format!("clock discontinuity in run(s) {}", disc_runs.join(", "))
        },
        disc_runs,
    );

    // Background processes that appear in some runs but not others.
    if !included.is_empty() {
        let mut present_in: std::collections::BTreeMap<&str, Vec<String>> =
            std::collections::BTreeMap::new();
        for r in &included {
            for name in &r.process_names {
                present_in
                    .entry(name.as_str())
                    .or_default()
                    .push(r.id.clone());
            }
        }
        let total = included.len();
        let mut partial: Vec<(String, Vec<String>)> = present_in
            .into_iter()
            .filter(|(_, runs)| runs.len() < total)
            .map(|(name, runs)| (name.to_string(), runs))
            .collect();
        partial.sort_by(|a, b| a.0.cmp(&b.0));
        partial.truncate(8);
        if !partial.is_empty() {
            let evidence = partial
                .iter()
                .map(|(name, runs)| format!("{name} only in {}", runs.join(", ")))
                .collect::<Vec<_>>()
                .join("; ");
            push(
                "background_processes",
                "Background processes",
                "changed",
                format!("possible background confounders: {evidence}"),
                partial.into_iter().flat_map(|(_, r)| r).collect(),
            );
        } else {
            push(
                "background_processes",
                "Background processes",
                "controlled",
                "the same processes were observed in every run".to_string(),
                Vec::new(),
            );
        }
    }

    out
}

type ConfounderPush<'a> =
    dyn FnMut(&'static str, &'static str, &'static str, String, Vec<String>) + 'a;

fn push_variation(
    key: &'static str,
    label: &'static str,
    values: &[Option<String>],
    run_ids: &[String],
    push: &mut ConfounderPush<'_>,
) {
    if values.iter().any(|v| v.is_none()) {
        push(
            key,
            label,
            "unknown",
            "not recorded for every run".to_string(),
            Vec::new(),
        );
        return;
    }
    let known: Vec<String> = values.iter().flatten().cloned().collect();
    let mut distinct = known.clone();
    distinct.sort();
    distinct.dedup();
    if distinct.len() <= 1 {
        push(
            key,
            label,
            "controlled",
            "same on every run".to_string(),
            Vec::new(),
        );
        return;
    }
    // The majority value names the reference; runs differing from it are the
    // ones that actually changed (never a blanket "all runs").
    let majority = most_common(&known);
    let offending: Vec<String> = values
        .iter()
        .zip(run_ids)
        .filter(|(v, _)| v.as_deref() != Some(majority.as_str()))
        .map(|(_, id)| id.clone())
        .collect();
    push(
        key,
        label,
        "changed",
        format!("varies across runs ({})", distinct.join(" / ")),
        offending,
    );
}

fn most_common(values: &[String]) -> String {
    let mut best = String::new();
    let mut best_count = 0usize;
    for candidate in values {
        let count = values.iter().filter(|v| *v == candidate).count();
        if count > best_count {
            best_count = count;
            best = candidate.clone();
        }
    }
    best
}

fn sets_equal(sets: Vec<Vec<String>>) -> bool {
    if sets.is_empty() {
        return true;
    }
    let mut normalized: Vec<Vec<String>> = sets
        .into_iter()
        .map(|mut s| {
            s.sort();
            s.dedup();
            s
        })
        .collect();
    let first = normalized.pop();
    normalized.into_iter().all(|s| Some(s) == first)
}

// ---------------------------------------------------------------------------
// Secondary metrics
// ---------------------------------------------------------------------------

fn secondary_comparison(runs: &[RunOutcome], primary: &str) -> Vec<SecondaryComparison> {
    let curated = compare::curated_metric_keys();
    let included: Vec<&RunOutcome> = runs.iter().filter(|r| r.included).collect();
    let mut out = Vec::new();
    for key in &curated {
        if *key == primary {
            continue;
        }
        let mut a: Vec<f64> = Vec::new();
        let mut b: Vec<f64> = Vec::new();
        let mut label = String::new();
        let mut unit = String::new();
        for r in &included {
            if let Some((_, l, u, Some(v))) = r.secondary.iter().find(|(k, _, _, _)| k == key) {
                if label.is_empty() {
                    label = l.clone();
                    unit = u.clone();
                }
                if r.group == RunGroup::Baseline {
                    a.push(*v);
                } else {
                    b.push(*v);
                }
            }
        }
        if a.is_empty() && b.is_empty() {
            continue;
        }
        if label.is_empty() {
            label = (*key).to_string();
        }
        let (med_a, med_b) = (stats::median(&a), stats::median(&b));
        let delta = match (med_a, med_b) {
            (Some(x), Some(y)) => Some(y - x),
            _ => None,
        };
        let comparable = med_a.is_some() && med_b.is_some();
        let note = if comparable {
            "Observed alongside the primary result; correlation is not causation.".to_string()
        } else {
            "Not present on both groups; not comparable.".to_string()
        };
        out.push(SecondaryComparison {
            metric: (*key).to_string(),
            label,
            unit,
            baseline_median: med_a,
            treatment_median: med_b,
            delta,
            comparable,
            note,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Wording
// ---------------------------------------------------------------------------

fn fmt_value(v: Option<f64>, unit: &str) -> String {
    match v {
        Some(v) if unit.is_empty() => format!("{v:.3}"),
        Some(v) => format!("{v:.3} {unit}"),
        None => "—".to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn summary_lines(
    config: &ExperimentConfig,
    baseline: &GroupSummary,
    treatment: &GroupSummary,
    effect: &EffectEstimate,
    noise: &NoiseAssessment,
    paired: Option<&PairedReport>,
    classification: ExperimentResult,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "Baseline: {} median {} across {} run(s).",
        config.primary_label,
        fmt_value(effect.baseline_value, &config.primary_unit),
        baseline.runs_with_primary
    ));
    lines.push(format!(
        "Treatment: {} median {} across {} run(s).",
        config.primary_label,
        fmt_value(effect.treatment_value, &config.primary_unit),
        treatment.runs_with_primary
    ));
    if let Some(p) = paired
        && p.usable_pairs >= 2
        && let (Some(med), Some(spread)) = (p.median_delta, p.spread)
    {
        lines.push(format!(
            "Median paired difference {med:+.3} {} across {} pair(s); pair spread {spread:.3}.",
            config.primary_unit, p.usable_pairs
        ));
        if p.consistent {
            lines.push(format!(
                "All {} paired treatment runs moved in the same direction as the median.",
                p.usable_pairs
            ));
        }
    }
    if let Some(delta) = effect.absolute_delta {
        let rel = effect
            .relative_delta
            .map(|r| format!(" ({:+.1}%)", r * 100.0))
            .unwrap_or_default();
        let word = match effect.direction {
            "lower" => "lower",
            "higher" => "higher",
            _ => "different",
        };
        lines.push(format!(
            "Treatment runs showed {:.3} {} {word} {} than baseline{rel}.",
            delta.abs(),
            config.primary_unit,
            config.primary_label
        ));
    }
    let (n_a, n_b) = (baseline.runs_with_primary, treatment.runs_with_primary);
    let small_n = n_a.min(n_b);
    if small_n <= 1 {
        lines.push(
            "Only one usable run per group: exploratory only, no reproducibility evidence."
                .to_string(),
        );
    } else if small_n == 2 {
        lines.push("Two usable runs per group: very weak reproducibility evidence.".to_string());
    } else if small_n == 3 {
        lines.push("Three usable runs per group: limited reproducibility evidence.".to_string());
    }
    match classification {
        ExperimentResult::ConsistentDifference => lines.push(
            "The observed difference exceeded the observed run-to-run variation.".to_string(),
        ),
        ExperimentResult::WithinNoise => {
            lines.push("The observed difference is within run-to-run noise.".to_string())
        }
        ExperimentResult::NoDifferenceDetected => lines.push(
            "No measurable difference was detected; this does not prove no difference exists.".to_string(),
        ),
        ExperimentResult::PossibleDifference => lines.push(
            "A possible difference was observed, but it is not decisive against run-to-run noise.".to_string(),
        ),
        ExperimentResult::Confounded => lines.push(
            "The comparison is confounded; the observed difference cannot be attributed to the treatment."
                .to_string(),
        ),
        ExperimentResult::Insufficient => lines.push(
            "Insufficient runs for a defensible between-group conclusion.".to_string(),
        ),
    }
    lines.push(noise.note.clone());
    if config.direction != TreatmentDirection::Neutral && effect.direction != "unchanged" {
        lines.push(
            "Desired direction is defined for wording only; it does not change the statistics."
                .to_string(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{BatteryPoint, SessionData};
    use crate::telemetry::{ClockStamp, Telemetry};

    fn stamp(ms: u64) -> ClockStamp {
        ClockStamp {
            wall_millis: ms,
            mono_millis: ms,
        }
    }

    /// A 60 s session whose discharge power has the given level. `distinct`
    /// controls how many distinct readings the sensor produced (0 = constant).
    fn session(w: f64, distinct: usize) -> SessionData {
        let mut s = SessionData {
            label: "t".to_string(),
            wall_base_ms: 1000,
            ..Default::default()
        };
        for i in 0..60u64 {
            let v = if distinct == 0 {
                w
            } else {
                w + (i as usize % distinct.max(1)) as f64 * 0.1
            };
            s.battery.push(BatteryPoint {
                t: i as f64,
                discharge: Telemetry::measured(v, "battery", stamp(1000 + i * 1000)),
                ..Default::default()
            });
        }
        s
    }

    fn cfg() -> ExperimentConfig {
        ExperimentConfig {
            primary_metric: "battery_discharge_w".to_string(),
            primary_label: "Battery discharge".to_string(),
            primary_unit: "W".to_string(),
            direction: TreatmentDirection::Lower,
            pairing: Pairing::Unpaired,
        }
    }

    fn run<'a>(
        s: &'a SessionData,
        id: &str,
        group: RunGroup,
        order: usize,
        pair: Option<usize>,
        included: bool,
    ) -> RunInput<'a> {
        RunInput {
            meta: RunMeta {
                id: id.to_string(),
                label: id.to_string(),
                group,
                order,
                pair_id: pair,
                included,
            },
            input: ComparisonInput::whole(s, 1000),
        }
    }

    #[test]
    fn identical_groups_show_no_difference() {
        let a1 = session(8.0, 5);
        let a2 = session(8.0, 5);
        let b1 = session(8.0, 5);
        let b2 = session(8.0, 5);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&a2, "A2", RunGroup::Baseline, 2, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
            run(&b2, "B2", RunGroup::Treatment, 3, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert_eq!(out.effect.absolute_delta, Some(0.0));
        assert_eq!(out.classification, ExperimentResult::NoDifferenceDetected);
    }

    #[test]
    fn clear_repeated_difference_is_consistent() {
        let a1 = session(11.0, 0);
        let a2 = session(11.0, 0);
        let a3 = session(11.0, 0);
        let b1 = session(9.0, 0);
        let b2 = session(9.0, 0);
        let b3 = session(9.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
            run(&a2, "A2", RunGroup::Baseline, 2, None, true),
            run(&b2, "B2", RunGroup::Treatment, 3, None, true),
            run(&a3, "A3", RunGroup::Baseline, 4, None, true),
            run(&b3, "B3", RunGroup::Treatment, 5, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert_eq!(out.effect.absolute_delta, Some(-2.0));
        assert_eq!(out.classification, ExperimentResult::ConsistentDifference);
        assert!(out.effect.relative_delta.unwrap() < -0.15);
        assert!(out.summary_lines.iter().any(|l| l.contains("lower")));
    }

    #[test]
    fn run_level_unit_is_the_run_not_the_sample() {
        // Add thousands of samples to one run; experimental N must stay at the
        // number of runs, never the number of telemetry ticks.
        let mut dense = session(9.0, 0);
        for i in 60..6000u64 {
            dense.battery.push(BatteryPoint {
                t: i as f64 / 100.0,
                discharge: Telemetry::measured(9.0, "battery", stamp(1000 + i * 10)),
                ..Default::default()
            });
        }
        let a1 = session(11.0, 0);
        let a2 = session(11.0, 0);
        let b1 = session(9.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&a2, "A2", RunGroup::Baseline, 1, None, true),
            run(&b1, "B1", RunGroup::Treatment, 2, None, true),
            run(&dense, "B2", RunGroup::Treatment, 3, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert_eq!(out.baseline.runs_with_primary, 2);
        assert_eq!(out.treatment.runs_with_primary, 2);
        // The huge sample count must not create a large N anywhere.
        assert_eq!(out.baseline.runs_included + out.treatment.runs_included, 4);
    }

    #[test]
    fn n_of_one_is_insufficient() {
        let a1 = session(11.0, 0);
        let b1 = session(8.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert_eq!(out.classification, ExperimentResult::Insufficient);
        assert_eq!(out.validity, ExperimentValidity::Insufficient);
        assert!(
            out.summary_lines
                .iter()
                .any(|l| l.contains("exploratory only"))
        );
        assert!(!out.summary_lines.iter().any(|l| l.contains("definitely")));
    }

    #[test]
    fn paired_runs_report_pair_differences() {
        let a1 = session(11.0, 0);
        let a2 = session(10.0, 0);
        let b1 = session(9.0, 0);
        let b2 = session(8.0, 0);
        let mut config = cfg();
        config.pairing = Pairing::Paired;
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, Some(1), true),
            run(&b1, "B1", RunGroup::Treatment, 1, Some(1), true),
            run(&a2, "A2", RunGroup::Baseline, 2, Some(2), true),
            run(&b2, "B2", RunGroup::Treatment, 3, Some(2), true),
        ];
        let out = analyze_experiment(&runs, &config);
        let paired = out.paired.unwrap();
        assert_eq!(paired.usable_pairs, 2);
        assert_eq!(paired.pairs[0].delta, -2.0);
        assert_eq!(paired.pairs[1].delta, -2.0);
        assert!(paired.consistent);
    }

    #[test]
    fn unpaired_does_not_invent_pairing() {
        let a1 = session(11.0, 0);
        let b1 = session(9.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert!(out.paired.is_none());
    }

    #[test]
    fn low_coverage_run_is_flagged_not_deleted() {
        let mut bad = session(9.0, 0);
        bad.battery.retain(|p| p.t < 2.0);
        let a1 = session(11.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&bad, "B1", RunGroup::Treatment, 1, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        let b1 = out.runs.iter().find(|r| r.id == "B1").unwrap();
        assert_eq!(b1.validation.status, "invalid");
        assert!(out.caveats.iter().any(|c| c.contains("failed quality")));
        // Kept visible.
        assert!(out.runs.iter().any(|r| r.id == "B1"));
    }

    #[test]
    fn excluded_run_is_held_out_but_visible() {
        let a1 = session(11.0, 0);
        let a2 = session(11.0, 0);
        let b1 = session(9.0, 0);
        let b2 = session(2.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&a2, "A2", RunGroup::Baseline, 1, None, true),
            run(&b1, "B1", RunGroup::Treatment, 2, None, true),
            run(&b2, "B2", RunGroup::Treatment, 3, None, false),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert_eq!(out.treatment.runs_included, 1);
        assert_eq!(out.treatment.runs_excluded, 1);
        assert_eq!(out.effect.treatment_value, Some(9.0));
        assert!(out.runs.iter().any(|r| r.id == "B2" && !r.included));
    }

    #[test]
    fn power_source_change_is_a_critical_confounder() {
        let a1 = session(11.0, 0);
        let a2 = session(11.0, 0);
        let b1 = session(9.0, 0);
        let b2 = session(9.0, 0);
        // Mark B runs as AC via battery_extra; A runs battery.
        let mut a1 = a1;
        add_ac(&mut a1, 0.0);
        let mut a2 = a2;
        add_ac(&mut a2, 0.0);
        let mut b1 = b1;
        add_ac(&mut b1, 1.0);
        let mut b2 = b2;
        add_ac(&mut b2, 1.0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
            run(&a2, "A2", RunGroup::Baseline, 2, None, true),
            run(&b2, "B2", RunGroup::Treatment, 3, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert_eq!(out.validity, ExperimentValidity::Confounded);
        assert_eq!(out.classification, ExperimentResult::Confounded);
        assert!(
            out.confounders
                .iter()
                .any(|c| c.key == "power_source" && c.state == "changed")
        );
    }

    fn add_ac(s: &mut SessionData, ac: f64) {
        use crate::session::BatteryExtraPoint;
        for i in 0..60u64 {
            s.battery_extra.push(BatteryExtraPoint {
                t: i as f64,
                ac: Telemetry::measured(ac, "battery", stamp(1000 + i * 1000)),
                ..Default::default()
            });
        }
    }

    #[test]
    fn cadence_mismatch_is_reported() {
        let a1 = session(11.0, 0);
        let a2 = session(11.0, 0);
        let mut b1 = session(9.0, 0);
        // Re-space B at 250 ms so its cadence differs sharply.
        for (i, p) in b1.battery.iter_mut().enumerate() {
            p.t = i as f64 * 0.25;
            p.discharge = Telemetry::measured(9.0, "battery", stamp(1000 + i as u64 * 250));
        }
        let b2 = session(9.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
            run(&a2, "A2", RunGroup::Baseline, 2, None, true),
            run(&b2, "B2", RunGroup::Treatment, 3, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert!(
            out.confounders
                .iter()
                .any(|c| c.key == "cadence" && c.state == "changed")
        );
    }

    #[test]
    fn missing_primary_metric_is_insufficient_not_switched() {
        let mut a1 = session(11.0, 0);
        a1.battery.clear();
        let a2 = session(11.0, 0);
        let b1 = session(9.0, 0);
        let b2 = session(9.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
            run(&a2, "A2", RunGroup::Baseline, 2, None, true),
            run(&b2, "B2", RunGroup::Treatment, 3, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert_eq!(out.primary_metric, "battery_discharge_w");
        assert!(
            out.caveats
                .iter()
                .any(|c| c.contains("no Battery discharge"))
        );
        assert!(
            out.runs
                .iter()
                .any(|r| r.id == "A1" && !r.validation.primary_present)
        );
    }

    #[test]
    fn zero_denominator_withholds_relative_delta() {
        let a1 = session(0.0, 0);
        let a2 = session(0.0, 0);
        let b1 = session(2.0, 0);
        let b2 = session(2.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
            run(&a2, "A2", RunGroup::Baseline, 2, None, true),
            run(&b2, "B2", RunGroup::Treatment, 3, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert!(out.effect.relative_delta.is_none());
    }

    #[test]
    fn difference_within_noise_is_classified_as_such() {
        let a1 = session(10.0, 0);
        let a2 = session(12.0, 0);
        let a3 = session(11.0, 0);
        let b1 = session(10.2, 0);
        let b2 = session(12.2, 0);
        let b3 = session(11.2, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
            run(&a2, "A2", RunGroup::Baseline, 2, None, true),
            run(&b2, "B2", RunGroup::Treatment, 3, None, true),
            run(&a3, "A3", RunGroup::Baseline, 4, None, true),
            run(&b3, "B3", RunGroup::Treatment, 5, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        assert!(out.effect.absolute_delta.unwrap().abs() < 0.5);
        assert!(
            matches!(
                out.classification,
                ExperimentResult::WithinNoise | ExperimentResult::PossibleDifference
            ),
            "{:?}",
            out.classification
        );
    }

    #[test]
    fn confounder_classification_states_are_explicit() {
        let a1 = session(11.0, 0);
        let a2 = session(11.0, 0);
        let b1 = session(9.0, 0);
        let b2 = session(9.0, 0);
        let runs = vec![
            run(&a1, "A1", RunGroup::Baseline, 0, None, true),
            run(&b1, "B1", RunGroup::Treatment, 1, None, true),
            run(&a2, "A2", RunGroup::Baseline, 2, None, true),
            run(&b2, "B2", RunGroup::Treatment, 3, None, true),
        ];
        let out = analyze_experiment(&runs, &cfg());
        // Power source is unknown because battery_extra was not recorded.
        let ps = out
            .confounders
            .iter()
            .find(|c| c.key == "power_source")
            .unwrap();
        assert_eq!(ps.state, "unknown");
        // Cadence is controlled (all 1 s).
        let cad = out.confounders.iter().find(|c| c.key == "cadence").unwrap();
        assert_eq!(cad.state, "controlled");
    }
}
