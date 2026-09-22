//! Experiments bridge: versioned local persistence plus the adapter between
//! persisted run references and the pure `pf_core::experiment` engine.
//!
//! Raw sessions are never copied into an experiment file. A run stores a
//! session path plus an optional range; the session remains authoritative. If
//! a referenced session disappears the run is marked unavailable and kept
//! visible, never silently dropped.
//!
//! Per-run reductions are cached by session fingerprint + range + primary
//! metric, so toggling run inclusion only recomputes the between-run
//! statistics instead of re-parsing every session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use pf_core::experiment as core;

use crate::dto::{
    BridgeError, CachedResultDto, ConfounderDto, CreateExperimentRequest, EffectEstimateDto,
    ExperimentAnalysisDto, ExperimentContextDto, ExperimentRecordDto, ExperimentSummaryDto,
    GroupSummaryDto, NoiseAssessmentDto, PairedDifferenceDto, PairedReportDto, RunOutcomeDto,
    RunRecordDto, RunValidationDto, SecondaryComparisonDto, SecondaryValueDto, UnavailableRunDto,
};
use crate::index::Fingerprint;
use crate::sessions;

/// Persisted schema version. Bump only for an incompatible file change.
pub const EXPERIMENT_SCHEMA: u32 = 1;
const MAX_CACHE_ENTRIES: usize = 2048;

fn experiments_dir(sessions_dir: &Path) -> PathBuf {
    sessions_dir.join("experiments")
}

/// Directory experiments are persisted in (for Diagnostics).
pub fn store_dir(sessions_dir: &Path) -> PathBuf {
    experiments_dir(sessions_dir)
}

/// Count of persisted experiment records without parsing them (Diagnostics).
pub fn store_count(sessions_dir: &Path) -> usize {
    std::fs::read_dir(experiments_dir(sessions_dir))
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

fn record_path(sessions_dir: &Path, id: &str) -> Result<PathBuf, BridgeError> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(BridgeError::new("bad-request", "invalid experiment id"));
    }
    Ok(experiments_dir(sessions_dir).join(format!("{id}.json")))
}

fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "exp-{}-{}",
        now_ms(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

pub fn list(sessions_dir: &Path) -> Result<Vec<ExperimentSummaryDto>, BridgeError> {
    let dir = experiments_dir(sessions_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|x| x != "json").unwrap_or(true) {
            continue;
        }
        if let Ok(record) = load_record(&path) {
            out.push(summary(sessions_dir, &record));
        }
    }
    out.sort_by(|a, b| {
        b.updated_ms
            .cmp(&a.updated_ms)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(out)
}

fn load_record(path: &Path) -> Result<ExperimentRecordDto, BridgeError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        BridgeError::new("io-error", format!("cannot read {}", path.display()))
            .with_detail(e.to_string())
    })?;
    serde_json::from_str(&text).map_err(|e| {
        BridgeError::new("experiment-corrupt", "experiment file is not valid JSON")
            .with_detail(e.to_string())
    })
}

pub fn get(sessions_dir: &Path, id: &str) -> Result<ExperimentRecordDto, BridgeError> {
    let path = record_path(sessions_dir, id)?;
    if !path.exists() {
        return Err(BridgeError::new(
            "not-found",
            format!("experiment {id} not found"),
        ));
    }
    load_record(&path)
}

pub fn save(
    sessions_dir: &Path,
    mut record: ExperimentRecordDto,
) -> Result<ExperimentRecordDto, BridgeError> {
    if record.name.trim().is_empty() {
        return Err(BridgeError::new(
            "bad-request",
            "experiment name is required",
        ));
    }
    if record.primary_metric.trim().is_empty() {
        return Err(BridgeError::new(
            "bad-request",
            "a primary metric is required",
        ));
    }
    if record.id.is_empty() {
        record.id = new_id();
    }
    if record.schema == 0 {
        record.schema = EXPERIMENT_SCHEMA;
    }
    if record.created_ms == 0 {
        record.created_ms = now_ms();
    }
    record.updated_ms = now_ms();
    let id = record.id.clone();
    let dir = experiments_dir(sessions_dir);
    std::fs::create_dir_all(&dir).map_err(|e| {
        BridgeError::new("io-error", "cannot create experiments directory")
            .with_detail(e.to_string())
    })?;
    let path = record_path(sessions_dir, &id)?;
    let tmp = dir.join(format!("{id}.json.tmp"));
    let text = serde_json::to_string_pretty(&record).map_err(|e| {
        BridgeError::new("internal", "cannot serialize experiment").with_detail(e.to_string())
    })?;
    let _guard = write_lock().lock();
    std::fs::write(&tmp, text).map_err(|e| {
        BridgeError::new("io-error", "cannot write experiment").with_detail(e.to_string())
    })?;
    let _ = std::fs::remove_file(&path);
    std::fs::rename(&tmp, &path).map_err(|e| {
        BridgeError::new("io-error", "cannot finalize experiment file").with_detail(e.to_string())
    })?;
    Ok(record)
}

pub fn create(
    sessions_dir: &Path,
    req: CreateExperimentRequest,
) -> Result<ExperimentRecordDto, BridgeError> {
    let now = now_ms();
    let record = ExperimentRecordDto {
        schema: EXPERIMENT_SCHEMA,
        id: new_id(),
        name: req.name,
        question: req.question,
        primary_metric: req.primary_metric,
        primary_label: req.primary_label,
        primary_unit: req.primary_unit,
        direction: req.direction,
        pairing: req.pairing,
        baseline_label: req.baseline_label,
        treatment_label: req.treatment_label,
        settle_s: req.settle_s,
        measure_s: req.measure_s,
        repetitions: req.repetitions,
        randomized: req.randomized,
        order_seed: req.randomized.then_some(now),
        collectors: req.collectors,
        preset: req.preset,
        notes: req.notes,
        status: "draft".to_string(),
        created_ms: now,
        updated_ms: now,
        runs: Vec::new(),
        guided: None,
        last_result: None,
    };
    save(sessions_dir, record)
}

pub fn delete(sessions_dir: &Path, id: &str) -> Result<(), BridgeError> {
    let path = record_path(sessions_dir, id)?;
    if !path.exists() {
        return Err(BridgeError::new(
            "not-found",
            format!("experiment {id} not found"),
        ));
    }
    std::fs::remove_file(&path).map_err(|e| {
        BridgeError::new("io-error", "cannot delete experiment").with_detail(e.to_string())
    })?;
    Ok(())
}

/// A referenced session is available only when it resolves inside the session
/// directory. Missing runs are counted, never dropped.
fn session_available(sessions_dir: &Path, stored: &str) -> bool {
    crate::resolve_session_path(sessions_dir, stored).is_ok()
}

fn summary(sessions_dir: &Path, r: &ExperimentRecordDto) -> ExperimentSummaryDto {
    let baseline = r.runs.iter().filter(|x| x.group == "baseline").count();
    let treatment = r.runs.iter().filter(|x| x.group == "treatment").count();
    // Empty paths are guided runs not recorded yet, not missing sessions.
    let missing = r
        .runs
        .iter()
        .filter(|x| !x.session_path.trim().is_empty())
        .filter(|x| !session_available(sessions_dir, &x.session_path))
        .count();
    ExperimentSummaryDto {
        id: r.id.clone(),
        name: r.name.clone(),
        question: r.question.clone(),
        primary_metric: r.primary_metric.clone(),
        primary_label: r.primary_label.clone(),
        primary_unit: r.primary_unit.clone(),
        baseline_label: r.baseline_label.clone(),
        treatment_label: r.treatment_label.clone(),
        status: r.status.clone(),
        baseline_runs: baseline,
        treatment_runs: treatment,
        included_runs: r.runs.iter().filter(|x| x.included).count(),
        missing_runs: missing,
        updated_ms: r.updated_ms,
        last_result: r.last_result.clone(),
    }
}

// ---------------------------------------------------------------------------
// Reduction cache
// ---------------------------------------------------------------------------

struct CachedRun {
    fingerprint: (u64, Option<u64>),
    outcome: core::RunOutcome,
}

fn run_cache() -> &'static Mutex<HashMap<String, CachedRun>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedRun>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(path: &Path, from: Option<u64>, to: Option<u64>, metric: &str) -> String {
    format!(
        "{}|{}|{}|{metric}",
        path.to_string_lossy(),
        from.unwrap_or(0),
        to.unwrap_or(0)
    )
}

fn config(record: &ExperimentRecordDto) -> core::ExperimentConfig {
    core::ExperimentConfig {
        primary_metric: record.primary_metric.clone(),
        primary_label: record.primary_label.clone(),
        primary_unit: record.primary_unit.clone(),
        direction: direction(&record.direction),
        pairing: pairing(&record.pairing),
    }
}

fn direction(s: &str) -> core::TreatmentDirection {
    match s {
        "higher" => core::TreatmentDirection::Higher,
        "neutral" => core::TreatmentDirection::Neutral,
        _ => core::TreatmentDirection::Lower,
    }
}

fn pairing(s: &str) -> core::Pairing {
    if s == "paired" {
        core::Pairing::Paired
    } else {
        core::Pairing::Unpaired
    }
}

fn group(s: &str) -> core::RunGroup {
    if s == "baseline" {
        core::RunGroup::Baseline
    } else {
        core::RunGroup::Treatment
    }
}

fn stamp(outcome: &mut core::RunOutcome, run: &RunRecordDto) {
    outcome.id = run.id.clone();
    outcome.label = if run.label.is_empty() {
        run.id.clone()
    } else {
        run.label.clone()
    };
    outcome.group = group(&run.group);
    outcome.order = run.order;
    outcome.pair_id = run.pair_id;
    outcome.included = run.included;
}

fn reduce_cached(
    sessions_dir: &Path,
    run: &RunRecordDto,
    cfg: &core::ExperimentConfig,
) -> Result<core::RunOutcome, BridgeError> {
    let path = crate::resolve_session_path(sessions_dir, &run.session_path)?;
    let fp = Fingerprint::of(&path).unwrap_or(Fingerprint {
        bytes: 0,
        mtime_s: 0,
    });
    let key = cache_key(&path, run.from_ms, run.to_ms, &cfg.primary_metric);
    if let Ok(cache) = run_cache().lock()
        && let Some(entry) = cache.get(&key)
        && entry.fingerprint == (fp.bytes, Some(fp.mtime_s))
    {
        let mut outcome = entry.outcome.clone();
        stamp(&mut outcome, run);
        return Ok(outcome);
    }
    let region =
        sessions::load_region_data(&path, run.from_ms.unwrap_or(0), run.to_ms.unwrap_or(0))?;
    let interval_ms = sessions::session_interval_ms(&path)?;
    let input = pf_core::compare::ComparisonInput {
        session: &region,
        interval_ms,
        from_ms: run.from_ms,
        to_ms: run.to_ms,
    };
    let meta = core::RunMeta {
        id: run.id.clone(),
        label: run.label.clone(),
        group: group(&run.group),
        order: run.order,
        pair_id: run.pair_id,
        included: run.included,
    };
    let run_input = core::RunInput { meta, input };
    let outcome = core::reduce_run(&run_input, cfg);
    if let Ok(mut cache) = run_cache().lock() {
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(
            key,
            CachedRun {
                fingerprint: (fp.bytes, Some(fp.mtime_s)),
                outcome: outcome.clone(),
            },
        );
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

pub fn analyze(sessions_dir: &Path, id: &str) -> Result<ExperimentAnalysisDto, BridgeError> {
    let mut record = get(sessions_dir, id)?;
    let cfg = config(&record);
    let mut outcomes: Vec<core::RunOutcome> = Vec::new();
    let mut unavailable: Vec<UnavailableRunDto> = Vec::new();
    for run in &record.runs {
        if run.session_path.trim().is_empty() {
            unavailable.push(UnavailableRunDto {
                id: run.id.clone(),
                label: run.label.clone(),
                group: run.group.clone(),
                order: run.order,
                session_path: String::new(),
                reason: "not recorded yet".to_string(),
            });
            continue;
        }
        match reduce_cached(sessions_dir, run, &cfg) {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => unavailable.push(UnavailableRunDto {
                id: run.id.clone(),
                label: run.label.clone(),
                group: run.group.clone(),
                order: run.order,
                session_path: run.session_path.clone(),
                reason: format!("{}: {}", e.code, e.message),
            }),
        }
    }
    let analysis = core::analyze_outcomes(outcomes, &cfg);
    let cached = CachedResultDto {
        analysis_version: analysis.analysis_version,
        classification: analysis.classification.as_str().to_string(),
        validity: analysis.validity.as_str().to_string(),
        absolute_delta: analysis.effect.absolute_delta,
        relative_delta: analysis.effect.relative_delta,
        computed_ms: now_ms(),
    };
    record.last_result = Some(cached);
    record.updated_ms = now_ms();
    let _ = save(sessions_dir, record);

    let mut dto = map_analysis(analysis);
    dto.unavailable_runs = unavailable;
    Ok(dto)
}

pub fn validate_run(
    sessions_dir: &Path,
    session_path: &str,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    primary_metric: &str,
    primary_label: &str,
    primary_unit: &str,
) -> Result<RunValidationDto, BridgeError> {
    if primary_metric.trim().is_empty() {
        return Err(BridgeError::new(
            "bad-request",
            "a primary metric is required",
        ));
    }
    let cfg = core::ExperimentConfig {
        primary_metric: primary_metric.to_string(),
        primary_label: primary_label.to_string(),
        primary_unit: primary_unit.to_string(),
        direction: core::TreatmentDirection::Lower,
        pairing: core::Pairing::Unpaired,
    };
    let record = RunRecordDto {
        id: "validate".to_string(),
        label: "validate".to_string(),
        group: "baseline".to_string(),
        order: 0,
        pair_id: None,
        session_path: session_path.to_string(),
        from_ms,
        to_ms,
        included: true,
        source: "guided".to_string(),
        notes: String::new(),
        captured_ms: None,
    };
    let outcome = reduce_cached(sessions_dir, &record, &cfg)?;
    Ok(RunValidationDto {
        status: outcome.validation.status.to_string(),
        primary_present: outcome.validation.primary_present,
        findings: outcome.validation.findings,
    })
}

fn map_analysis(a: core::ExperimentAnalysis) -> ExperimentAnalysisDto {
    ExperimentAnalysisDto {
        analysis_version: a.analysis_version,
        primary_metric: a.primary_metric,
        primary_label: a.primary_label,
        primary_unit: a.primary_unit,
        direction: match a.direction {
            core::TreatmentDirection::Lower => "lower",
            core::TreatmentDirection::Higher => "higher",
            core::TreatmentDirection::Neutral => "neutral",
        }
        .to_string(),
        pairing: a.pairing.as_str().to_string(),
        runs: a.runs.into_iter().map(map_run).collect(),
        baseline: map_group(a.baseline),
        treatment: map_group(a.treatment),
        effect: EffectEstimateDto {
            baseline_value: a.effect.baseline_value,
            treatment_value: a.effect.treatment_value,
            absolute_delta: a.effect.absolute_delta,
            relative_delta: a.effect.relative_delta,
            direction: a.effect.direction.to_string(),
            effect_size: a.effect.effect_size,
            confidence_interval: a.effect.confidence_interval,
            paired: a.effect.paired,
        },
        paired: a.paired.map(|p| PairedReportDto {
            pairs: p
                .pairs
                .into_iter()
                .map(|d| PairedDifferenceDto {
                    pair_id: d.pair_id,
                    baseline_run: d.baseline_run,
                    treatment_run: d.treatment_run,
                    baseline: d.baseline,
                    treatment: d.treatment,
                    delta: d.delta,
                })
                .collect(),
            usable_pairs: p.usable_pairs,
            median_delta: p.median_delta,
            spread: p.spread,
            consistent: p.consistent,
        }),
        noise: NoiseAssessmentDto {
            source: a.noise.source.to_string(),
            estimate: a.noise.estimate,
            note: a.noise.note,
        },
        classification: a.classification.as_str().to_string(),
        validity: a.validity.as_str().to_string(),
        validity_reasons: a.validity_reasons,
        summary_lines: a.summary_lines,
        confounders: a
            .confounders
            .into_iter()
            .map(|c| ConfounderDto {
                key: c.key.to_string(),
                label: c.label.to_string(),
                state: c.state.to_string(),
                evidence: c.evidence,
                runs: c.runs,
            })
            .collect(),
        secondary: a
            .secondary
            .into_iter()
            .map(|s| SecondaryComparisonDto {
                metric: s.metric,
                label: s.label,
                unit: s.unit,
                baseline_median: s.baseline_median,
                treatment_median: s.treatment_median,
                delta: s.delta,
                comparable: s.comparable,
                note: s.note,
            })
            .collect(),
        caveats: a.caveats,
        unavailable_runs: Vec::new(),
    }
}

fn map_run(r: core::RunOutcome) -> RunOutcomeDto {
    RunOutcomeDto {
        id: r.id,
        label: r.label,
        group: r.group.as_str().to_string(),
        order: r.order,
        pair_id: r.pair_id,
        included: r.included,
        available: true,
        unavailable_reason: None,
        from_ms: r.from_ms,
        to_ms: r.to_ms,
        duration_s: r.duration_s,
        primary_value: r.primary_value,
        primary_known: r.primary_known,
        coverage: r.coverage,
        discontinuities: r.discontinuities,
        timeouts: r.timeouts,
        cadence_ms: r.cadence_ms,
        context: ExperimentContextDto {
            power_scheme: r.context.power_scheme,
            power_source: r.context.power_source,
            refresh_hz: r.context.refresh_hz,
            display_count: r.context.display_count,
            gpu_adapters: r.context.gpu_adapters,
            effective_cadence_ms: r.context.effective_cadence_ms,
        },
        collectors: r.collectors,
        process_incomplete: r.process_incomplete,
        process_names: r.process_names,
        secondary: r
            .secondary
            .into_iter()
            .map(|(metric, label, unit, median)| SecondaryValueDto {
                metric,
                label,
                unit,
                median,
            })
            .collect(),
        validation: RunValidationDto {
            status: r.validation.status.to_string(),
            primary_present: r.validation.primary_present,
            findings: r.validation.findings,
        },
    }
}

fn map_group(g: core::GroupSummary) -> GroupSummaryDto {
    GroupSummaryDto {
        group: g.group.as_str().to_string(),
        runs_total: g.runs_total,
        runs_included: g.runs_included,
        runs_excluded: g.runs_excluded,
        runs_with_primary: g.runs_with_primary,
        runs_valid: g.runs_valid,
        runs_invalid: g.runs_invalid,
        median: g.median,
        mean: g.mean,
        spread: g.spread,
        min: g.min,
        max: g.max,
        coverage_median: g.coverage_median,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-gui-exp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session(path: &Path, base: f64) {
        let mut text = String::from(
            "{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000,\"note\":\"exp\"}\n",
        );
        for i in 0..60u64 {
            let v = base + (i % 5) as f64 * 0.2;
            text.push_str(&format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\
                 \"discharge_w\":{{\"v\":{v},\"p\":\"measured\"}}}}\n",
                1000 + i * 1000,
                i * 1000
            ));
        }
        text.push_str("{\"type\":\"session_footer\",\"wall_ms\":61000,\"summary\":{\"samples\":60,\"discharge_wh\":0.1,\"discharge_coverage_pct\":99.0,\"recovered\":false}}\n");
        std::fs::write(path, text).unwrap();
    }

    fn req(name: &str) -> CreateExperimentRequest {
        CreateExperimentRequest {
            name: name.to_string(),
            question: "does it help?".to_string(),
            primary_metric: "battery_discharge_w".to_string(),
            primary_label: "Battery discharge".to_string(),
            primary_unit: "W".to_string(),
            direction: "lower".to_string(),
            pairing: "unpaired".to_string(),
            baseline_label: "120 Hz".to_string(),
            treatment_label: "60 Hz".to_string(),
            settle_s: 30.0,
            measure_s: 120.0,
            repetitions: 2,
            randomized: false,
            collectors: None,
            preset: None,
            notes: String::new(),
        }
    }

    #[test]
    fn create_save_load_and_summary_roundtrip() {
        let dir = temp_dir("roundtrip");
        let record = create(&dir, req("refresh")).unwrap();
        assert!(record.id.starts_with("exp-"));
        assert_eq!(record.schema, EXPERIMENT_SCHEMA);
        let loaded = get(&dir, &record.id).unwrap();
        assert_eq!(loaded.name, "refresh");
        let items = list(&dir).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, "draft");
        // A session reference outside the dir never counts as available.
        let mut with_run = loaded.clone();
        with_run.runs.push(RunRecordDto {
            id: "A1".to_string(),
            label: "A1".to_string(),
            group: "baseline".to_string(),
            order: 0,
            pair_id: None,
            session_path: "C:\\Windows\\notepad.exe".to_string(),
            from_ms: None,
            to_ms: None,
            included: true,
            source: "existing".to_string(),
            notes: String::new(),
            captured_ms: None,
        });
        save(&dir, with_run).unwrap();
        let items = list(&dir).unwrap();
        assert_eq!(items[0].missing_runs, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn analyze_existing_sessions_reports_groups_and_caches_metadata() {
        let dir = temp_dir("analyze");
        write_session(&dir.join("a1.jsonl"), 11.0);
        write_session(&dir.join("a2.jsonl"), 11.0);
        write_session(&dir.join("b1.jsonl"), 9.0);
        write_session(&dir.join("b2.jsonl"), 9.0);
        let mut record = create(&dir, req("brightness")).unwrap();
        let run = |id: &str, group: &str, file: &str, order: usize| RunRecordDto {
            id: id.to_string(),
            label: id.to_string(),
            group: group.to_string(),
            order,
            pair_id: None,
            session_path: dir.join(file).to_string_lossy().to_string(),
            from_ms: None,
            to_ms: None,
            included: true,
            source: "existing".to_string(),
            notes: String::new(),
            captured_ms: None,
        };
        record.runs = vec![
            run("A1", "baseline", "a1.jsonl", 0),
            run("B1", "treatment", "b1.jsonl", 1),
            run("A2", "baseline", "a2.jsonl", 2),
            run("B2", "treatment", "b2.jsonl", 3),
        ];
        record.status = "complete".to_string();
        save(&dir, record.clone()).unwrap();

        let out = analyze(&dir, &record.id).unwrap();
        assert_eq!(out.baseline.runs_with_primary, 2);
        assert_eq!(out.treatment.runs_with_primary, 2);
        assert!(out.effect.absolute_delta.unwrap() < 0.0);
        // Missing sessions are surfaced, not dropped.
        assert!(out.unavailable_runs.is_empty());
        // Cached metadata is persisted for the library view.
        let reloaded = get(&dir, &record.id).unwrap();
        assert!(reloaded.last_result.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_session_is_reported_as_unavailable() {
        let dir = temp_dir("missing");
        write_session(&dir.join("a1.jsonl"), 11.0);
        let mut record = create(&dir, req("missing")).unwrap();
        record.runs = vec![
            RunRecordDto {
                id: "A1".to_string(),
                label: "A1".to_string(),
                group: "baseline".to_string(),
                order: 0,
                pair_id: None,
                session_path: dir.join("a1.jsonl").to_string_lossy().to_string(),
                from_ms: None,
                to_ms: None,
                included: true,
                source: "existing".to_string(),
                notes: String::new(),
                captured_ms: None,
            },
            RunRecordDto {
                id: "B1".to_string(),
                label: "B1".to_string(),
                group: "treatment".to_string(),
                order: 1,
                pair_id: None,
                session_path: dir.join("gone.jsonl").to_string_lossy().to_string(),
                from_ms: None,
                to_ms: None,
                included: true,
                source: "existing".to_string(),
                notes: String::new(),
                captured_ms: None,
            },
        ];
        save(&dir, record.clone()).unwrap();
        let out = analyze(&dir, &record.id).unwrap();
        assert_eq!(out.unavailable_runs.len(), 1);
        assert_eq!(out.unavailable_runs[0].id, "B1");
        assert!(out.runs.iter().all(|r| r.id != "B1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real-session timing harness. Run explicitly:
    /// `cargo test -p pf-gui --lib perf_real_experiment -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_real_experiment() {
        let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../sessions"));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("no sessions dir at {}", dir.display());
            return;
        };
        let mut files: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
            .collect();
        files.sort();
        if files.len() < 2 {
            eprintln!("need two sessions; found {}", files.len());
            return;
        }
        let mut record = create(&dir, req("perf")).unwrap();
        let mk = |id: &str, group: &str, order: usize, path: &std::path::Path| RunRecordDto {
            id: id.to_string(),
            label: id.to_string(),
            group: group.to_string(),
            order,
            pair_id: None,
            session_path: path.to_string_lossy().to_string(),
            from_ms: None,
            to_ms: None,
            included: true,
            source: "existing".to_string(),
            notes: String::new(),
            captured_ms: None,
        };
        record.runs = vec![
            mk("A1", "baseline", 0, &files[0]),
            mk("B1", "treatment", 1, &files[1]),
            mk("A2", "baseline", 2, &files[0]),
            mk("B2", "treatment", 3, &files[1]),
        ];
        save(&dir, record.clone()).unwrap();

        let t = std::time::Instant::now();
        let out = analyze(&dir, &record.id).unwrap();
        println!(
            "open + analyze (4 runs, cold): {:?} (class={}, runs={})",
            t.elapsed(),
            out.classification,
            out.runs.len()
        );
        let t = std::time::Instant::now();
        let _ = analyze(&dir, &record.id).unwrap();
        println!("recompute (warm run cache): {:?}", t.elapsed());

        let mut toggled = get(&dir, &record.id).unwrap();
        toggled.runs[3].included = false;
        save(&dir, toggled.clone()).unwrap();
        let t = std::time::Instant::now();
        let _ = analyze(&dir, &record.id).unwrap();
        println!("toggle run inclusion + recompute: {:?}", t.elapsed());

        let mut switched = get(&dir, &record.id).unwrap();
        switched.primary_metric = "cpu_utility_pct".to_string();
        switched.primary_label = "CPU utility".to_string();
        switched.primary_unit = "%".to_string();
        save(&dir, switched.clone()).unwrap();
        let t = std::time::Instant::now();
        let _ = analyze(&dir, &record.id).unwrap();
        println!("switch primary metric + recompute: {:?}", t.elapsed());
        let _ = delete(&dir, &record.id);
    }

    #[test]
    fn validate_run_flags_low_coverage() {
        let dir = temp_dir("validate");
        let mut text =
            String::from("{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000}\n");
        for i in 0..3u64 {
            text.push_str(&format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\
                 \"discharge_w\":{{\"v\":8.0,\"p\":\"measured\"}}}}\n",
                1000 + i * 1000,
                i * 1000
            ));
        }
        std::fs::write(dir.join("short.jsonl"), text).unwrap();
        let v = validate_run(
            &dir,
            &dir.join("short.jsonl").to_string_lossy(),
            None,
            None,
            "battery_discharge_w",
            "Battery discharge",
            "W",
        )
        .unwrap();
        assert_eq!(v.status, "invalid");
        assert!(!v.findings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_experiment_metadata_is_reported_not_fatal() {
        let dir = temp_dir("corrupt");
        let store = experiments_dir(&dir);
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("broken.json"), "{ not json").unwrap();
        // A corrupt record is skipped when listing, and named as corrupt when
        // opened directly, rather than panicking or taking down the page.
        assert!(list(&dir).unwrap().is_empty());
        let err = get(&dir, "broken").unwrap_err();
        assert_eq!(err.code, "experiment-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
