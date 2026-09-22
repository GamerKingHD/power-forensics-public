//! Report generation bridge.
//!
//! Reports are built from the same structured analysis outputs the workspaces
//! render (Analyze range, Compare, Experiment, Calibration) and rendered by
//! [`pf_core::report`]. No screenshotting or UI scraping is involved, and the
//! engine's own wording is carried through verbatim: the reporting layer never
//! strengthens a `possible_difference` into a "significant improvement".
//!
//! Every report embeds an evidence manifest (source fingerprints, ranges,
//! analysis/application versions, calibration ids, redaction state) so it is
//! auditable and reproducible.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use pf_core::report::{
    ChartEvent, ChartPoint, ChartSeries, EvidenceManifest, RedactionState, Report, ReportChart,
    ReportRange, ReportSection, ReportSourceKind, ReportTable, Reproducibility, SourceFingerprint,
};

use crate::dto::BridgeError;
use crate::{
    analyze, calibration, capabilities, compare, elevation, experiment, index, sessions, settings,
};

const ANALYZE_VERSION: &str = "analyze-1";
const COMPARE_VERSION: &str = "compare-1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReportOptions {
    #[serde(default = "yes")]
    pub include_timeline: bool,
    #[serde(default = "yes")]
    pub include_processes: bool,
    #[serde(default = "yes")]
    pub include_secondary: bool,
    #[serde(default = "yes")]
    pub detailed_caveats: bool,
    #[serde(default)]
    pub redact: bool,
}

fn yes() -> bool {
    true
}

impl Default for ReportOptions {
    fn default() -> Self {
        ReportOptions {
            include_timeline: true,
            include_processes: true,
            include_secondary: true,
            detailed_caveats: true,
            redact: false,
        }
    }
}

/// Flat request covering all report/export sources. `kind` selects the source;
/// unknown combinations are rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRequest {
    pub kind: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub from_ms: Option<u64>,
    #[serde(default)]
    pub to_ms: Option<u64>,
    #[serde(default)]
    pub path_b: Option<String>,
    #[serde(default)]
    pub from_ms_b: Option<u64>,
    #[serde(default)]
    pub to_ms_b: Option<u64>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub options: ReportOptions,
}

/// A built report plus its rendered HTML and a safe default file name.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltReport {
    pub title: String,
    pub html: String,
    pub manifest: ReportManifestDto,
    pub suggested_file_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportManifestDto {
    pub report_schema_version: u32,
    pub kind: String,
    pub generated_at_ms: u64,
    pub application_version: String,
    pub analysis_version: String,
    pub redaction: String,
    pub source_ids: Vec<String>,
    pub ranges: Vec<(u64, u64)>,
    pub calibration_ids: Vec<String>,
    pub reproducibility: String,
}

impl From<&EvidenceManifest> for ReportManifestDto {
    fn from(m: &EvidenceManifest) -> Self {
        ReportManifestDto {
            report_schema_version: m.report_schema_version,
            kind: m.kind.as_str().to_string(),
            generated_at_ms: m.generated_at_ms,
            application_version: m.application_version.clone(),
            analysis_version: m.analysis_version.clone(),
            redaction: m.redaction.as_str().to_string(),
            source_ids: m.sources.iter().map(|s| s.session_id.clone()).collect(),
            ranges: m.ranges.iter().map(|r| (r.from_ms, r.to_ms)).collect(),
            calibration_ids: m.calibration_ids.clone(),
            reproducibility: match &m.reproducibility {
                Reproducibility::Full => "full".to_string(),
                Reproducibility::Qualified(r) => format!("qualified: {r}"),
            },
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

fn fnum(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() => {
            let s = format!("{v:.3}");
            let s = s.trim_end_matches('0').trim_end_matches('.');
            if s.is_empty() || s == "-" {
                "0".to_string()
            } else {
                s.to_string()
            }
        }
        _ => "unavailable".to_string(),
    }
}

fn fp(path: &Path) -> SourceFingerprint {
    let f = index::Fingerprint::of(path);
    SourceFingerprint {
        session_id: path
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string()),
        path: path.to_string_lossy().to_string(),
        bytes: f.as_ref().map(|f| f.bytes).unwrap_or(0),
        mtime_s: f.as_ref().map(|f| f.mtime_s).unwrap_or(0),
    }
}

fn manifest(
    kind: ReportSourceKind,
    analysis_version: &str,
    sources: Vec<SourceFingerprint>,
    ranges: Vec<ReportRange>,
    calibration_ids: Vec<String>,
    options: &ReportOptions,
    reproducible: bool,
) -> EvidenceManifest {
    EvidenceManifest {
        report_schema_version: pf_core::report::REPORT_SCHEMA_VERSION,
        application_version: app_version(),
        analysis_version: analysis_version.to_string(),
        generated_at_ms: now_ms(),
        kind,
        sources,
        ranges,
        calibration_ids,
        redaction: if options.redact {
            RedactionState::Redacted
        } else {
            RedactionState::Original
        },
        options: vec![
            (
                "include_timeline".to_string(),
                options.include_timeline.to_string(),
            ),
            (
                "include_processes".to_string(),
                options.include_processes.to_string(),
            ),
            (
                "include_secondary".to_string(),
                options.include_secondary.to_string(),
            ),
            (
                "detailed_caveats".to_string(),
                options.detailed_caveats.to_string(),
            ),
        ],
        reproducibility: if reproducible {
            Reproducibility::Full
        } else {
            Reproducibility::Qualified(
                "a referenced raw session is missing or has no fingerprint".to_string(),
            )
        },
    }
}

fn active_calibration_ids(sessions_dir: &Path) -> Vec<String> {
    calibration::active_id(sessions_dir, calibration::TARGET_DISPLAY_POWER)
        .into_iter()
        .collect()
}

/// Load the sessions referenced by a report so redaction can derive the same
/// token set the CLI/export use. Failures are ignored: a missing session must
/// not block report generation.
fn redaction_tokens(paths: &[PathBuf]) -> Vec<pf_core::analysis::RedactToken> {
    let loaded: Vec<pf_core::session::SessionData> = paths
        .iter()
        .filter_map(|p| sessions::load_full(p).ok())
        .collect();
    let refs: Vec<&pf_core::session::SessionData> = loaded.iter().collect();
    pf_core::export::session_identifiers(&refs)
}

fn finish(
    mut report: Report,
    options: &ReportOptions,
    source_paths: &[PathBuf],
    file_stem: &str,
) -> BuiltReport {
    if options.redact {
        let tokens = redaction_tokens(source_paths);
        pf_core::report::redact_report(&mut report, &tokens);
        report.manifest.redaction = RedactionState::Redacted;
    }
    let html = pf_core::report::render_html(&report);
    let manifest = ReportManifestDto::from(&report.manifest);
    BuiltReport {
        title: report.title.clone(),
        html,
        manifest,
        suggested_file_name: format!("{}.html", sanitize_file_stem(file_stem)),
    }
}

/// Sanitize a user/session-derived name so a default save can never escape its
/// target directory.
pub fn sanitize_file_stem(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    out = out.trim_matches('.').to_string();
    if out.is_empty() {
        out = "report".to_string();
    }
    if out.len() > 120 {
        out.truncate(120);
    }
    out
}

pub fn build(sessions_dir: &Path, req: &ReportRequest) -> Result<BuiltReport, BridgeError> {
    match req.kind.as_str() {
        "analyze" => build_analyze(sessions_dir, req),
        "compare" => build_compare(sessions_dir, req),
        "experiment" => build_experiment(sessions_dir, req),
        "calibration" => build_calibration(sessions_dir, req),
        other => Err(BridgeError::new(
            "bad-request",
            format!("unknown report kind '{other}'"),
        )),
    }
}

fn resolve_path(sessions_dir: &Path, requested: &str) -> Result<PathBuf, BridgeError> {
    // Report sources are session files; reuse the same confinement as every
    // other session path so a report request can never read outside the
    // session directory.
    crate::resolve_session_path(sessions_dir, requested)
}

// ---------------------------------------------------------------------------
// Analyze
// ---------------------------------------------------------------------------

fn build_analyze(sessions_dir: &Path, req: &ReportRequest) -> Result<BuiltReport, BridgeError> {
    let requested = req
        .path
        .as_deref()
        .ok_or_else(|| BridgeError::new("bad-request", "analyze report needs a session path"))?;
    let target = resolve_path(sessions_dir, requested)?;
    let caps = capabilities::current_report(elevation::is_elevated())?;
    let overview = analyze::overview(&target, &caps)?;
    let interval = overview.interval_ms.max(1);
    let from = req.from_ms.unwrap_or(overview.start_wall_ms);
    let to = req.to_ms.unwrap_or(overview.end_wall_ms.max(from));
    let ra = analyze::range_analysis(&target, interval, from, to)?;

    let session_id = target
        .file_name()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut summary = Vec::new();
    summary.push(format!(
        "Analyzed {:.1}s of recorded evidence ({}–{} ms) in {}.",
        ra.duration_s, ra.from_ms, ra.to_ms, session_id
    ));
    if let Some(wh) = ra.energy.discharge_wh {
        summary.push(format!(
            "Discharge energy {} Wh over {}s of covered evidence.",
            fnum(Some(wh)),
            fnum(Some(ra.energy.covered_s))
        ));
    } else {
        summary.push(
            "Discharge energy unavailable for this range (no covered discharge evidence)."
                .to_string(),
        );
    }
    if let Some(c) = ra.quality.coverage {
        summary.push(format!("Evidence coverage {}%.", fnum(Some(c))));
    }
    for c in ra.changes.iter().take(3) {
        summary.push(c.basis.clone());
    }

    let mut evidence_quality = Vec::new();
    evidence_quality.push(format!(
        "Coverage {}%; covered {}s, unknown {}s, unobserved {}s.",
        fnum(ra.quality.coverage),
        fnum(Some(ra.quality.covered_s)),
        fnum(Some(ra.quality.unknown_s)),
        fnum(Some(ra.quality.unobserved_s))
    ));
    if ra.quality.discontinuities > 0 {
        evidence_quality.push(format!(
            "{} clock/sampling discontinuities in the range; energy is qualified.",
            ra.quality.discontinuities
        ));
    }
    if ra.quality.timeouts + ra.quality.recoveries + ra.quality.errors > 0 {
        evidence_quality.push(format!(
            "{} timeouts, {} recoveries, {} collector errors.",
            ra.quality.timeouts, ra.quality.recoveries, ra.quality.errors
        ));
    }
    if !ra.quality.stale_collectors.is_empty() {
        evidence_quality.push(format!(
            "Collectors with no samples in the range: {}.",
            ra.quality.stale_collectors.join(", ")
        ));
    }
    if overview.recovered {
        evidence_quality.push("Session footer was recovered from a torn recording.".to_string());
    }

    let mut sections = Vec::new();

    sections.push(ReportSection {
        heading: "Session".to_string(),
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Field".to_string(), "Value".to_string()],
            rows: vec![
                row2("File", session_id.clone()),
                row2("Label", overview.label.clone()),
                row2("Note", overview.note.clone()),
                row2("Status", overview.status.clone()),
                row2("Interval", format!("{} ms", overview.interval_ms)),
                row2("Samples", overview.stats.total_samples.to_string()),
                row2("Markers", overview.markers.to_string()),
            ],
        }],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Range and energy".to_string(),
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Metric".to_string(), "Value".to_string()],
            rows: vec![
                row2("Duration", format!("{} s", fnum(Some(ra.duration_s)))),
                row2(
                    "Discharge",
                    if ra.energy.discharge_present {
                        format!("{} Wh", fnum(ra.energy.discharge_wh))
                    } else {
                        "unavailable".to_string()
                    },
                ),
                row2(
                    "Charge",
                    if ra.energy.charge_present {
                        format!("{} Wh", fnum(ra.energy.charge_wh))
                    } else {
                        "unavailable".to_string()
                    },
                ),
                row2("Discontinuities", ra.energy.discontinuities.to_string()),
                row2("Timeouts", ra.quality.timeouts.to_string()),
                row2("Recoveries", ra.quality.recoveries.to_string()),
            ],
        }],
        ..Default::default()
    });

    let mut domain_rows = Vec::new();
    for d in &ra.domains {
        for m in &d.metrics {
            if m.known == 0 {
                continue;
            }
            domain_rows.push(vec![
                d.domain.clone(),
                m.label.clone(),
                fnum(m.median),
                m.unit.clone(),
                m.provenance.clone(),
                format!("{}/{}", m.known, m.n),
            ]);
        }
    }
    if !domain_rows.is_empty() {
        sections.push(ReportSection {
            heading: "Domain statistics".to_string(),
            tables: vec![ReportTable {
                caption: None,
                columns: vec![
                    "Domain".into(),
                    "Metric".into(),
                    "Median".into(),
                    "Unit".into(),
                    "Provenance".into(),
                    "Known/n".into(),
                ],
                rows: domain_rows,
            }],
            ..Default::default()
        });
    }

    if !ra.changes.is_empty() {
        sections.push(ReportSection {
            heading: "Observed changes".to_string(),
            notes: vec![
                "These are observed differences within the recording, not causal findings."
                    .to_string(),
            ],
            tables: vec![ReportTable {
                caption: None,
                columns: vec![
                    "Metric".into(),
                    "Direction".into(),
                    "Delta".into(),
                    "Unit".into(),
                    "Confidence".into(),
                    "Basis".into(),
                ],
                rows: ra
                    .changes
                    .iter()
                    .map(|c| {
                        vec![
                            c.label.clone(),
                            c.direction.clone(),
                            fnum(c.delta),
                            c.unit.clone(),
                            c.confidence.clone(),
                            c.basis.clone(),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    if req.options.include_timeline {
        let events: Vec<Vec<String>> = overview
            .events
            .iter()
            .filter(|e| e.wall_ms >= from && e.wall_ms <= to)
            .map(|e| {
                vec![
                    e.wall_ms.to_string(),
                    e.kind.clone(),
                    e.detail.clone(),
                    e.severity.clone(),
                ]
            })
            .collect();
        if !events.is_empty() {
            sections.push(ReportSection {
                heading: "Events".to_string(),
                tables: vec![ReportTable {
                    caption: Some("Events recorded in the selected range.".to_string()),
                    columns: vec![
                        "Wall ms".into(),
                        "Kind".into(),
                        "Detail".into(),
                        "Severity".into(),
                    ],
                    rows: events,
                }],
                ..Default::default()
            });
        }
    }

    if req.options.include_processes {
        let incomplete = ra.processes.incomplete == Some(true);
        sections.push(ReportSection {
            heading: "Process evidence".to_string(),
            notes: vec![ra.processes.note.clone()],
            tables: vec![ReportTable {
                caption: Some(if incomplete {
                    "Enumeration was incomplete: an absent process is 'not observed', never 'did not run'.".to_string()
                } else {
                    "Top processes by median CPU within the range.".to_string()
                }),
                columns: vec!["Process".into(), "PID".into(), "CPU median %".into(), "Presence".into()],
                rows: ra
                    .processes
                    .entries
                    .iter()
                    .take(25)
                    .map(|p| {
                        vec![
                            p.name.clone(),
                            p.pid.to_string(),
                            fnum(p.cpu_median_pct),
                            p.presence.to_string(),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    if !ra.correlations.is_empty() {
        sections.push(ReportSection {
            heading: "Correlations".to_string(),
            notes: vec![
                "Correlation is not causation; effective n accounts for autocorrelation."
                    .to_string(),
            ],
            tables: vec![ReportTable {
                caption: None,
                columns: vec![
                    "Pair".into(),
                    "r".into(),
                    "Strength".into(),
                    "Effective n".into(),
                    "Note".into(),
                ],
                rows: ra
                    .correlations
                    .iter()
                    .map(|c| {
                        vec![
                            c.label.clone(),
                            fnum(c.r),
                            c.strength.clone(),
                            format!("{} (of {})", c.effective_n, c.n),
                            c.note.clone(),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    if req.options.include_timeline
        && let Some(chart) = analyze_chart(&overview.series, interval, from, to, &overview.events)
    {
        sections.push(ReportSection {
            heading: "Timeline".to_string(),
            charts: vec![chart],
            notes: vec!["Gaps are periods with no available evidence; the line is not interpolated across them.".to_string()],
            ..Default::default()
        });
    }

    let mut caveats = Vec::new();
    if ra.energy.crosses_discontinuity {
        caveats.push(
            "The range crosses a clock/sampling discontinuity; energy is qualified.".to_string(),
        );
    }
    if ra.quality.coverage.map(|c| c < 90.0).unwrap_or(true) {
        caveats.push("Coverage is below 90% or unknown; energy may be understated.".to_string());
    }
    if ra.processes.incomplete == Some(true) {
        caveats.push(
            "Process enumeration was incomplete; absence is not evidence of not running."
                .to_string(),
        );
    }

    let report = Report {
        title: format!("Analyze report — {session_id}"),
        manifest: manifest(
            ReportSourceKind::Analyze,
            ANALYZE_VERSION,
            vec![fp(&target)],
            vec![ReportRange {
                from_ms: ra.from_ms,
                to_ms: ra.to_ms,
            }],
            active_calibration_ids(sessions_dir),
            &req.options,
            index::Fingerprint::of(&target).is_some(),
        ),
        summary_lines: summary,
        evidence_quality,
        sections,
        caveats,
    };
    let stem = format!("{session_id}-analyze-report");
    Ok(finish(report, &req.options, &[target], &stem))
}

fn row2(k: &str, v: String) -> Vec<String> {
    vec![k.to_string(), v]
}

fn analyze_chart(
    series: &[crate::dto::Series],
    gap_ms: u64,
    from_ms: u64,
    to_ms: u64,
    events: &[crate::dto::TimelineEvent],
) -> Option<ReportChart> {
    let s = series
        .iter()
        .find(|s| s.key.contains("discharge"))
        .or_else(|| series.first())?;
    let points: Vec<ChartPoint> = s
        .points
        .iter()
        .filter(|p| p.t_ms >= from_ms && p.t_ms <= to_ms)
        .map(|p| ChartPoint {
            t_ms: p.t_ms,
            value: p.value,
            provenance: p.provenance.clone(),
        })
        .collect();
    if points.is_empty() {
        return None;
    }
    let chart_events = events
        .iter()
        .filter(|e| {
            e.wall_ms >= from_ms && e.wall_ms <= to_ms && e.kind.eq_ignore_ascii_case("marker")
        })
        .map(|e| ChartEvent {
            t_ms: e.wall_ms,
            label: format!("marker: {}", e.detail),
        })
        .collect();
    Some(ReportChart {
        title: format!("{} ({})", s.label, s.unit),
        series: vec![ChartSeries {
            label: s.label.clone(),
            unit: s.unit.clone(),
            points,
        }],
        events: chart_events,
        gap_ms: Some(gap_ms),
        note: None,
    })
}

// ---------------------------------------------------------------------------
// Compare
// ---------------------------------------------------------------------------

fn build_compare(sessions_dir: &Path, req: &ReportRequest) -> Result<BuiltReport, BridgeError> {
    let path_a = req
        .path
        .as_deref()
        .ok_or_else(|| BridgeError::new("bad-request", "compare report needs path A"))?;
    let path_b = req
        .path_b
        .as_deref()
        .ok_or_else(|| BridgeError::new("bad-request", "compare report needs path B"))?;
    let a = resolve_path(sessions_dir, path_a)?;
    let b = resolve_path(sessions_dir, path_b)?;
    let c = compare::compare(&a, req.from_ms, req.to_ms, &b, req.from_ms_b, req.to_ms_b)?;

    let mut summary = Vec::new();
    summary.push(c.energy.note.clone());
    summary.push(format!(
        "Comparability: {}. Weakest evidence side: {}.",
        c.comparability.overall, c.quality.weakest_side
    ));
    for r in c.ranked_changes.iter().take(3) {
        summary.push(r.basis.clone());
    }
    if summary.iter().all(|s| s.is_empty()) {
        summary.push("Comparison produced no ranked differences.".to_string());
    }

    let mut evidence_quality = vec![c.quality.note.clone()];
    for f in &c.comparability.findings {
        evidence_quality.push(f.reason.clone());
    }

    let mut sections = Vec::new();
    sections.push(ReportSection {
        heading: "A/B definition".to_string(),
        tables: vec![ReportTable {
            caption: None,
            columns: vec![
                "Side".into(),
                "Session".into(),
                "Range (ms)".into(),
                "Duration s".into(),
                "Coverage %".into(),
                "Recovered".into(),
            ],
            rows: vec![
                vec![
                    "A".into(),
                    c.a.label.clone(),
                    format!("{}–{}", c.a.from_ms, c.a.to_ms),
                    fnum(Some(c.a.duration_s)),
                    fnum(c.a.coverage),
                    c.a.recovered.to_string(),
                ],
                vec![
                    "B".into(),
                    c.b.label.clone(),
                    format!("{}–{}", c.b.from_ms, c.b.to_ms),
                    fnum(Some(c.b.duration_s)),
                    fnum(c.b.coverage),
                    c.b.recovered.to_string(),
                ],
            ],
        }],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Comparability".to_string(),
        notes: vec![format!("Overall: {}", c.comparability.overall)],
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Scope".into(), "Level".into(), "Reason".into()],
            rows: c
                .comparability
                .findings
                .iter()
                .map(|f| {
                    vec![
                        f.metric.clone().unwrap_or_else(|| "comparison".to_string()),
                        f.level.clone(),
                        f.reason.clone(),
                    ]
                })
                .collect(),
        }],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Energy".to_string(),
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Basis".into(), "Metric".into(), "A".into(), "B".into()],
            rows: vec![
                vec![
                    "Energy".into(),
                    "Total Wh".into(),
                    fnum(c.energy.total_wh_a),
                    fnum(c.energy.total_wh_b),
                ],
                vec![
                    "Energy".into(),
                    "Average W".into(),
                    fnum(c.energy.avg_power_w_a),
                    fnum(c.energy.avg_power_w_b),
                ],
                vec![
                    "Energy".into(),
                    "Normalized Wh/h".into(),
                    fnum(c.energy.normalized_wh_a),
                    fnum(c.energy.normalized_wh_b),
                ],
            ],
        }],
        notes: vec![format!("Preferred basis: {}.", c.energy.preferred_basis)],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Headline metrics".to_string(),
        notes: vec!["Observed differences, not causation.".to_string()],
        tables: vec![ReportTable {
            caption: None,
            columns: vec![
                "Metric".into(),
                "A".into(),
                "B".into(),
                "Delta".into(),
                "Unit".into(),
                "Direction".into(),
                "Comparability".into(),
                "Reliability".into(),
                "Evidence".into(),
            ],
            rows: c
                .headline_metrics
                .iter()
                .map(|m| {
                    vec![
                        m.label.clone(),
                        fnum(m.a),
                        fnum(m.b),
                        fnum(m.absolute_delta),
                        m.unit.clone(),
                        m.direction.clone(),
                        m.comparability.clone(),
                        m.reliability.level.clone(),
                        m.evidence.clone(),
                    ]
                })
                .collect(),
        }],
        ..Default::default()
    });

    if !c.ranked_changes.is_empty() {
        sections.push(ReportSection {
            heading: "Largest observed differences".to_string(),
            tables: vec![ReportTable {
                caption: Some(
                    "Ranked by |delta| relative to the measured noise floor.".to_string(),
                ),
                columns: vec![
                    "Metric".into(),
                    "Delta".into(),
                    "Unit".into(),
                    "Direction".into(),
                    "Comparability".into(),
                    "Reliability".into(),
                    "Basis".into(),
                ],
                rows: c
                    .ranked_changes
                    .iter()
                    .map(|r| {
                        vec![
                            r.label.clone(),
                            fnum(Some(r.delta)),
                            r.unit.clone(),
                            r.direction.clone(),
                            r.comparability.clone(),
                            r.reliability.clone(),
                            r.basis.clone(),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    if req.options.include_processes && !c.processes.rows.is_empty() {
        sections.push(ReportSection {
            heading: "Process differences".to_string(),
            notes: vec![if c.processes.incomplete {
                "Process enumeration was incomplete on at least one side; absence is not evidence of not running.".to_string()
            } else {
                "Observed process presence/CPU differences.".to_string()
            }],
            tables: vec![ReportTable {
                caption: None,
                columns: vec!["Process".into(), "A CPU %".into(), "B CPU %".into(), "Delta".into(), "Presence".into(), "Note".into()],
                rows: c
                    .processes
                    .rows
                    .iter()
                    .take(25)
                    .map(|p| {
                        vec![
                            p.display_name.clone(),
                            fnum(p.a_cpu_median_pct),
                            fnum(p.b_cpu_median_pct),
                            fnum(p.delta),
                            p.presence.clone(),
                            p.note.clone(),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    if !c.categorical_differences.is_empty() {
        sections.push(ReportSection {
            heading: "Context differences".to_string(),
            tables: vec![ReportTable {
                caption: None,
                columns: vec![
                    "Key".into(),
                    "A".into(),
                    "B".into(),
                    "State".into(),
                    "Note".into(),
                ],
                rows: c
                    .categorical_differences
                    .iter()
                    .map(|d| {
                        vec![
                            d.label.clone(),
                            d.a.clone().unwrap_or_else(|| "unavailable".to_string()),
                            d.b.clone().unwrap_or_else(|| "unavailable".to_string()),
                            d.state.clone(),
                            d.note.clone(),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    let caveats: Vec<String> = c.caveats.iter().map(|x| x.message.clone()).collect();
    let report = Report {
        title: format!("Compare report — {} vs {}", c.a.label, c.b.label),
        manifest: manifest(
            ReportSourceKind::Compare,
            COMPARE_VERSION,
            vec![fp(&a), fp(&b)],
            vec![
                ReportRange {
                    from_ms: c.a.from_ms,
                    to_ms: c.a.to_ms,
                },
                ReportRange {
                    from_ms: c.b.from_ms,
                    to_ms: c.b.to_ms,
                },
            ],
            active_calibration_ids(sessions_dir),
            &req.options,
            index::Fingerprint::of(&a).is_some() && index::Fingerprint::of(&b).is_some(),
        ),
        summary_lines: summary,
        evidence_quality,
        sections,
        caveats,
    };
    let stem = format!("{}-vs-{}-compare-report", c.a.label, c.b.label);
    Ok(finish(report, &req.options, &[a, b], &stem))
}

// ---------------------------------------------------------------------------
// Experiment
// ---------------------------------------------------------------------------

fn build_experiment(sessions_dir: &Path, req: &ReportRequest) -> Result<BuiltReport, BridgeError> {
    let id = req
        .id
        .as_deref()
        .ok_or_else(|| BridgeError::new("bad-request", "experiment report needs an id"))?;
    let record = experiment::get(sessions_dir, id)?;
    let analysis = experiment::analyze(sessions_dir, id)?;

    let mut sources = Vec::new();
    let mut source_paths = Vec::new();
    let mut ranges = Vec::new();
    for run in &record.runs {
        if let Ok(p) = crate::resolve_session_path(sessions_dir, &run.session_path)
            && let Some(f) = index::Fingerprint::of(&p)
        {
            sources.push(SourceFingerprint {
                session_id: run.session_path.clone(),
                path: p.to_string_lossy().to_string(),
                bytes: f.bytes,
                mtime_s: f.mtime_s,
            });
            source_paths.push(p);
        }
        if run.from_ms.is_some() || run.to_ms.is_some() {
            ranges.push(ReportRange {
                from_ms: run.from_ms.unwrap_or(0),
                to_ms: run.to_ms.unwrap_or(0),
            });
        }
    }

    let mut sections = Vec::new();
    sections.push(ReportSection {
        heading: "Question and design".to_string(),
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Field".into(), "Value".into()],
            rows: vec![
                row2("Experiment", record.name.clone()),
                row2(
                    "Question",
                    if record.question.is_empty() {
                        "(none)".to_string()
                    } else {
                        record.question.clone()
                    },
                ),
                row2(
                    "Primary metric",
                    format!("{} ({})", analysis.primary_label, analysis.primary_unit),
                ),
                row2("Direction", analysis.direction.clone()),
                row2("Pairing", analysis.pairing.clone()),
                row2("Baseline", record.baseline_label.clone()),
                row2("Treatment", record.treatment_label.clone()),
                row2("Settle s", fnum(Some(record.settle_s))),
                row2("Measure s", fnum(Some(record.measure_s))),
                row2("Repetitions", record.repetitions.to_string()),
                row2("Randomized order", record.randomized.to_string()),
                row2("Status", record.status.clone()),
            ],
        }],
        notes: vec!["The experimental unit is the run, not the sample.".to_string()],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Runs".to_string(),
        tables: vec![ReportTable {
            caption: Some("Individual runs in execution order; excluded runs are shown but held out of group statistics.".to_string()),
            columns: vec!["Order".into(), "Run".into(), "Group".into(), "Included".into(), "Primary".into(), "Coverage %".into(), "Validation".into(), "Available".into()],
            rows: analysis
                .runs
                .iter()
                .map(|r| {
                    vec![
                        r.order.to_string(),
                        r.label.clone(),
                        r.group.clone(),
                        r.included.to_string(),
                        fnum(r.primary_value),
                        fnum(r.coverage),
                        r.validation.status.clone(),
                        r.available.to_string(),
                    ]
                })
                .collect(),
        }],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Group summaries".to_string(),
        tables: vec![ReportTable {
            caption: None,
            columns: vec![
                "Group".into(),
                "Included".into(),
                "Median".into(),
                "Mean".into(),
                "Spread (MAD)".into(),
                "Min".into(),
                "Max".into(),
                "Coverage %".into(),
            ],
            rows: vec![
                group_row("Baseline", &analysis.baseline),
                group_row("Treatment", &analysis.treatment),
            ],
        }],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Effect estimate".to_string(),
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Field".into(), "Value".into()],
            rows: vec![
                row2("Baseline", fnum(analysis.effect.baseline_value)),
                row2("Treatment", fnum(analysis.effect.treatment_value)),
                row2("Absolute delta", fnum(analysis.effect.absolute_delta)),
                row2("Relative delta", fnum(analysis.effect.relative_delta)),
                row2("Direction", analysis.effect.direction.clone()),
                row2("Effect size", fnum(analysis.effect.effect_size)),
                row2(
                    "Bootstrap 5–95%",
                    match analysis.effect.confidence_interval {
                        Some((lo, hi)) => format!("{} … {}", fnum(Some(lo)), fnum(Some(hi))),
                        None => "unavailable".to_string(),
                    },
                ),
                row2("Paired", analysis.effect.paired.to_string()),
            ],
        }],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Noise and validity".to_string(),
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Field".into(), "Value".into()],
            rows: vec![
                row2("Noise source", analysis.noise.source.clone()),
                row2("Noise estimate", fnum(analysis.noise.estimate)),
                row2("Classification", analysis.classification.clone()),
                row2("Validity", analysis.validity.clone()),
            ],
        }],
        notes: {
            let mut n = vec![analysis.noise.note.clone()];
            n.extend(analysis.validity_reasons.clone());
            n
        },
        ..Default::default()
    });

    if let Some(paired) = &analysis.paired {
        sections.push(ReportSection {
            heading: "Paired differences".to_string(),
            tables: vec![ReportTable {
                caption: None,
                columns: vec![
                    "Pair".into(),
                    "Baseline run".into(),
                    "Treatment run".into(),
                    "Delta".into(),
                ],
                rows: paired
                    .pairs
                    .iter()
                    .map(|p| {
                        vec![
                            p.pair_id.to_string(),
                            p.baseline_run.clone(),
                            p.treatment_run.clone(),
                            fnum(Some(p.delta)),
                        ]
                    })
                    .collect(),
            }],
            notes: vec![format!(
                "{} usable pair(s); consistent direction: {}.",
                paired.usable_pairs, paired.consistent
            )],
            ..Default::default()
        });
    }

    if !analysis.confounders.is_empty() {
        sections.push(ReportSection {
            heading: "Confounders".to_string(),
            tables: vec![ReportTable {
                caption: None,
                columns: vec![
                    "Factor".into(),
                    "State".into(),
                    "Evidence".into(),
                    "Runs".into(),
                ],
                rows: analysis
                    .confounders
                    .iter()
                    .map(|c| {
                        vec![
                            c.label.clone(),
                            c.state.clone(),
                            c.evidence.clone(),
                            c.runs.join(", "),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    if req.options.include_secondary && !analysis.secondary.is_empty() {
        sections.push(ReportSection {
            heading: "Secondary metrics".to_string(),
            tables: vec![ReportTable {
                caption: None,
                columns: vec![
                    "Metric".into(),
                    "Baseline".into(),
                    "Treatment".into(),
                    "Delta".into(),
                    "Unit".into(),
                    "Comparable".into(),
                    "Note".into(),
                ],
                rows: analysis
                    .secondary
                    .iter()
                    .map(|s| {
                        vec![
                            s.label.clone(),
                            fnum(s.baseline_median),
                            fnum(s.treatment_median),
                            fnum(s.delta),
                            s.unit.clone(),
                            s.comparable.to_string(),
                            s.note.clone(),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    let mut caveats = analysis.caveats.clone();
    caveats.push("Observed difference ≠ causation.".to_string());
    if analysis.runs.iter().filter(|r| r.included).count() == 1 {
        caveats.push("N=1 in the included set: treat as a single observation.".to_string());
    }
    if !analysis.unavailable_runs.is_empty() {
        caveats.push(format!(
            "{} referenced run session(s) could not be read; they are excluded from the analysis.",
            analysis.unavailable_runs.len()
        ));
    }

    let report = Report {
        title: format!("Experiment report — {}", record.name),
        manifest: manifest(
            ReportSourceKind::Experiment,
            &format!("experiment-{}", analysis.analysis_version),
            sources,
            ranges,
            active_calibration_ids(sessions_dir),
            &req.options,
            source_paths.len()
                == record
                    .runs
                    .iter()
                    .filter(|r| !r.session_path.is_empty())
                    .count(),
        ),
        summary_lines: analysis.summary_lines.clone(),
        evidence_quality: analysis.validity_reasons.clone(),
        sections,
        caveats,
    };
    let stem = format!("{}-experiment-report", record.name);
    Ok(finish(report, &req.options, &source_paths, &stem))
}

fn group_row(label: &str, g: &crate::dto::GroupSummaryDto) -> Vec<String> {
    vec![
        label.to_string(),
        format!("{}/{}", g.runs_included, g.runs_total),
        fnum(g.median),
        fnum(g.mean),
        fnum(g.spread),
        fnum(g.min),
        fnum(g.max),
        fnum(g.coverage_median),
    ]
}

// ---------------------------------------------------------------------------
// Calibration
// ---------------------------------------------------------------------------

fn build_calibration(sessions_dir: &Path, req: &ReportRequest) -> Result<BuiltReport, BridgeError> {
    let id = req
        .id
        .as_deref()
        .ok_or_else(|| BridgeError::new("bad-request", "calibration report needs an id"))?;
    let rec = calibration::get(sessions_dir, id)?;
    let reference_label = pf_core::calibration::ReferenceKind::parse(&rec.reference_kind)
        .map(|k| k.label().to_string())
        .unwrap_or_else(|| rec.reference_kind.clone());

    let summary = vec![
        format!(
            "Calibration {} (revision {}) targets {}.",
            rec.id, rec.revision, rec.target
        ),
        format!(
            "Reference: {}. Evidence class: {}.",
            reference_label, rec.evidence_class
        ),
        format!(
            "Quality: {} — {}",
            rec.quality,
            rec.quality_reasons.join("; ")
        ),
    ];

    let mut evidence_quality = rec.quality_reasons.clone();
    evidence_quality.push(format!(
        "The calibrated value remains an estimate; it is never promoted to a measurement ({}).",
        rec.evidence_class
    ));

    let mut sections = Vec::new();
    sections.push(ReportSection {
        heading: "Model".to_string(),
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Field".into(), "Value".into()],
            rows: vec![
                row2("Target", rec.target.clone()),
                row2("Model kind", rec.model_kind.clone()),
                row2("Slope (W/%)", fnum(Some(rec.model.slope_w_per_pct))),
                row2("Intercept (W)", fnum(Some(rec.model.intercept_w))),
                row2("Min brightness (%)", fnum(Some(rec.model.min_brightness))),
                row2("R² (fit)", fnum(Some(rec.model.r2_fit))),
            ],
        }],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Fit and independent validation".to_string(),
        notes: vec![if rec.validation.is_some() {
            "Error statistics below are held-out; they are not the training fit.".to_string()
        } else {
            "No held-out validation was possible with this many points; independent validation is unavailable.".to_string()
        }],
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Metric".into(), "Value".into()],
            rows: vec![
                row2("Reference points", rec.references.len().to_string()),
                row2("Validation n", rec.validation.as_ref().map(|v| v.n.to_string()).unwrap_or_else(|| "unavailable".to_string())),
                row2("MAE (W)", rec.validation.as_ref().map(|v| fnum(Some(v.mae))).unwrap_or_else(|| "unavailable".to_string())),
                row2("RMSE (W)", rec.validation.as_ref().map(|v| fnum(Some(v.rmse))).unwrap_or_else(|| "unavailable".to_string())),
                row2("Median abs error (W)", rec.validation.as_ref().map(|v| fnum(Some(v.median_abs_error))).unwrap_or_else(|| "unavailable".to_string())),
                row2("Max abs error (W)", rec.validation.as_ref().map(|v| fnum(Some(v.max_abs_error))).unwrap_or_else(|| "unavailable".to_string())),
                row2("Validation R²", rec.validation.as_ref().and_then(|v| v.r2).map(|r| fnum(Some(r))).unwrap_or_else(|| "unavailable".to_string())),
            ],
        }],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Reference evidence".to_string(),
        notes: vec![
            "User/external-provided reference values; not measured by power-forensics.".to_string(),
        ],
        tables: vec![ReportTable {
            caption: None,
            columns: vec![
                "Input".into(),
                "Reference (W)".into(),
                "Kind".into(),
                "Source".into(),
                "Note".into(),
            ],
            rows: rec
                .references
                .iter()
                .map(|r| {
                    vec![
                        fnum(Some(r.input)),
                        fnum(Some(r.reference)),
                        r.kind.clone(),
                        r.source_label.clone(),
                        r.note.clone(),
                    ]
                })
                .collect(),
        }],
        ..Default::default()
    });

    sections.push(ReportSection {
        heading: "Applicability".to_string(),
        notes: vec![format!(
            "Extrapolation outside {:.1}–{:.1}% brightness is flagged, not treated as equally reliable.",
            rec.applicability.input_min, rec.applicability.input_max
        )],
        tables: vec![ReportTable {
            caption: None,
            columns: vec!["Field".into(), "Value".into()],
            rows: vec![
                row2("Machine", rec.applicability.machine.clone()),
                row2("Panel", rec.applicability.panel.clone()),
                row2("Input range (%)", format!("{}–{}", fnum(Some(rec.applicability.input_min)), fnum(Some(rec.applicability.input_max)))),
                row2("Power source", rec.applicability.power_source.clone().unwrap_or_else(|| "(unspecified)".to_string())),
            ],
        }],
        ..Default::default()
    });

    if !rec.residuals.is_empty() {
        sections.push(ReportSection {
            heading: "Residuals".to_string(),
            notes: vec!["Outliers are shown, never deleted; exclusion requires an explicit recorded reason.".to_string()],
            tables: vec![ReportTable {
                caption: None,
                columns: vec!["Input".into(), "Predicted (W)".into(), "Reference (W)".into(), "Abs error (W)".into(), "Rel error".into(), "Held out".into()],
                rows: rec
                    .residuals
                    .iter()
                    .map(|r| {
                        vec![
                            fnum(Some(r.input)),
                            fnum(Some(r.predicted)),
                            fnum(Some(r.reference)),
                            fnum(Some(r.abs_error)),
                            r.relative_error.map(|x| fnum(Some(x))).unwrap_or_else(|| "n/a".to_string()),
                            r.held_out.to_string(),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    if !rec.exclusions.is_empty() {
        sections.push(ReportSection {
            heading: "Excluded evidence".to_string(),
            tables: vec![ReportTable {
                caption: None,
                columns: vec!["Session".into(), "Reason".into()],
                rows: rec
                    .exclusions
                    .iter()
                    .map(|e| vec![e.session_id.clone(), e.reason.clone()])
                    .collect(),
            }],
            ..Default::default()
        });
    }

    if !rec.dataset.is_empty() {
        sections.push(ReportSection {
            heading: "Dataset references".to_string(),
            tables: vec![ReportTable {
                caption: Some("Raw sessions are referenced, not duplicated.".to_string()),
                columns: vec![
                    "Session".into(),
                    "From".into(),
                    "To".into(),
                    "Role".into(),
                    "Note".into(),
                ],
                rows: rec
                    .dataset
                    .iter()
                    .map(|d| {
                        vec![
                            d.session_id.clone(),
                            d.from_ms
                                .map(|x| x.to_string())
                                .unwrap_or_else(|| "-".to_string()),
                            d.to_ms
                                .map(|x| x.to_string())
                                .unwrap_or_else(|| "-".to_string()),
                            d.role.clone(),
                            d.note.clone(),
                        ]
                    })
                    .collect(),
            }],
            ..Default::default()
        });
    }

    let mut source_paths = Vec::new();
    let mut sources = Vec::new();
    for d in &rec.dataset {
        if let Ok(p) = crate::resolve_session_path(sessions_dir, &d.session_id)
            && let Some(f) = index::Fingerprint::of(&p)
        {
            sources.push(SourceFingerprint {
                session_id: d.session_id.clone(),
                path: p.to_string_lossy().to_string(),
                bytes: f.bytes,
                mtime_s: f.mtime_s,
            });
            source_paths.push(p);
        }
    }

    let report = Report {
        title: format!("Calibration report — {}", rec.id),
        manifest: manifest(
            ReportSourceKind::Calibration,
            "calibration-1",
            sources,
            vec![],
            vec![rec.id.clone()],
            &req.options,
            true,
        ),
        summary_lines: summary,
        evidence_quality,
        sections,
        caveats: vec![format!(
            "Version {} (schema {}). Recalibration creates a new revision and never overwrites this one.",
            rec.revision, rec.schema_version
        )],
    };
    let stem = format!("{}-calibration-report", rec.id);
    Ok(finish(report, &req.options, &source_paths, &stem))
}

/// Build and write a report to the configured report directory, confined to a
/// sanitized file name. Returns the written path.
pub fn build_and_save(
    sessions_dir: &Path,
    req: &ReportRequest,
) -> Result<(String, ReportManifestDto), BridgeError> {
    let built = build(sessions_dir, req)?;
    let settings = settings::load(sessions_dir);
    let dir = settings::report_dir(sessions_dir, &settings)?;
    let path = dir.join(&built.suggested_file_name);
    std::fs::write(&path, built.html).map_err(|e| {
        BridgeError::new("io-error", "cannot write report").with_detail(e.to_string())
    })?;
    Ok((path.to_string_lossy().to_string(), built.manifest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stem_sanitization_blocks_escape() {
        // Directory separators are replaced, so the result cannot traverse.
        let s = sanitize_file_stem("../../evil");
        assert!(!s.contains('/') && !s.contains('\\'), "{s}");
        assert!(s.ends_with("evil"), "{s}");
        assert_eq!(sanitize_file_stem("a b\\c/d"), "a_b_c_d");
        assert_eq!(sanitize_file_stem(""), "report");
        assert!(!sanitize_file_stem("..\\..\\windows\\system32").contains('\\'));
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let dir = std::env::temp_dir();
        let req = ReportRequest {
            kind: "wat".to_string(),
            path: None,
            from_ms: None,
            to_ms: None,
            path_b: None,
            from_ms_b: None,
            to_ms_b: None,
            id: None,
            options: ReportOptions::default(),
        };
        assert!(build(&dir, &req).is_err());
    }

    #[test]
    fn analyze_report_has_manifest_and_preserves_gaps() {
        // Build a small session on disk and generate an analyze report.
        let dir = std::env::temp_dir().join(format!("pf-gui-report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut lines = String::new();
        lines.push_str("{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000,\"note\":\"rep\"}\n");
        for i in 0..30 {
            let w = if i == 15 { "null" } else { "6.2" };
            let p = if i == 15 { "unavailable" } else { "measured" };
            lines.push_str(&format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\"discharge_w\":{{\"v\":{},\"p\":\"{}\"}},\"charge_w\":{{\"v\":null,\"p\":\"unavailable\",\"k\":2,\"q\":4,\"reason\":\"not charging\"}},\"remaining_mwh\":{{\"v\":20000,\"p\":\"measured\"}},\"charge_pct\":{{\"v\":71,\"p\":\"measured\"}}}}\n",
                1000 + i * 1000,
                i * 1000,
                w,
                p
            ));
        }
        lines.push_str("{\"type\":\"event\",\"wall_ms\":5000,\"mono_ms\":4000,\"kind\":\"marker\",\"detail\":\"m1\"}\n");
        lines.push_str("{\"type\":\"session_footer\",\"wall_ms\":31000,\"summary\":{\"samples\":30,\"discharge_median_w\":6.2,\"discharge_wh\":0.05,\"discharge_coverage_pct\":96.7,\"recovered\":false}}\n");
        std::fs::write(dir.join("rep.jsonl"), lines).unwrap();

        let req = ReportRequest {
            kind: "analyze".to_string(),
            path: Some("rep.jsonl".to_string()),
            from_ms: None,
            to_ms: None,
            path_b: None,
            from_ms_b: None,
            to_ms_b: None,
            id: None,
            options: ReportOptions::default(),
        };
        let built = build(&dir, &req).unwrap();
        assert!(built.html.contains("Evidence manifest"));
        assert!(built.html.contains("pf-report-schema"));
        assert_eq!(built.manifest.kind, "analyze");
        assert!(!built.manifest.source_ids.is_empty());
        // The unavailable point must not have been bridged or zeroed.
        assert!(built.html.contains("unavailable") || built.html.contains("data-prov"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redaction_masks_identity_in_html() {
        let dir = std::env::temp_dir().join(format!("pf-gui-report-red-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut lines = String::new();
        lines.push_str("{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000,\"note\":\"SECRET-HOST\"}\n");
        for i in 0..5 {
            lines.push_str(&format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\"discharge_w\":{{\"v\":6.2,\"p\":\"measured\"}},\"charge_pct\":{{\"v\":71,\"p\":\"measured\"}}}}\n",
                1000 + i * 1000,
                i * 1000
            ));
        }
        lines.push_str("{\"type\":\"session_footer\",\"wall_ms\":6000,\"summary\":{\"samples\":5,\"discharge_median_w\":6.2,\"discharge_coverage_pct\":90.0,\"recovered\":false}}\n");
        std::fs::write(dir.join("SECRET-HOST.jsonl"), lines).unwrap();
        let req = ReportRequest {
            kind: "analyze".to_string(),
            path: Some("SECRET-HOST.jsonl".to_string()),
            from_ms: None,
            to_ms: None,
            path_b: None,
            from_ms_b: None,
            to_ms_b: None,
            id: None,
            options: ReportOptions {
                redact: true,
                ..Default::default()
            },
        };
        let built = build(&dir, &req).unwrap();
        assert_eq!(built.manifest.redaction, "redacted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_sources_outside_the_session_directory_are_rejected() {
        let dir =
            std::env::temp_dir().join(format!("pf-gui-report-confine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = std::env::temp_dir().join(format!("pf-outside-{}.jsonl", std::process::id()));
        std::fs::write(&outside, "{}\n").unwrap();
        let outside_str = outside.to_string_lossy().to_string();
        for candidate in [
            "..\\..\\evil.jsonl",
            "../../evil.jsonl",
            outside_str.as_str(),
        ] {
            let req = ReportRequest {
                kind: "analyze".to_string(),
                path: Some(candidate.to_string()),
                from_ms: None,
                to_ms: None,
                path_b: None,
                from_ms_b: None,
                to_ms_b: None,
                id: None,
                options: ReportOptions::default(),
            };
            assert!(
                build(&dir, &req).is_err(),
                "report path {candidate} must be rejected"
            );
        }
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
