//! Machine-readable exports.
//!
//! Two contracts:
//! - a tidy long-format CSV (built by [`pf_core::export`]) whose missing-data
//!   representation is an empty `value` cell plus explicit
//!   `provenance`/`unavailable_kind`/`reason`/`stale` columns;
//! - a versioned JSON envelope for Analyze/Compare/Experiment/Calibration.
//!
//! The JSON envelope is the public export contract: it carries a stable
//! `schemaVersion` plus a manifest (sources, ranges, versions, calibration ids,
//! redaction) so a consumer never has to reverse-engineer internal structs.
//! Raw forensic evidence is never modified except by explicit redaction, which
//! reuses the backend redaction engine.

use std::io::BufWriter;
use std::path::Path;

use serde::Serialize;

use crate::dto::BridgeError;
use crate::report::{ReportRequest, sanitize_file_stem};
use crate::{
    analyze, calibration, capabilities, compare, elevation, experiment, index, sessions, settings,
};

/// Version of the JSON export envelope. Bump on any envelope shape change.
pub const EXPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportSource {
    session_id: String,
    path: String,
    bytes: u64,
    mtime_s: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportEnvelope {
    schema_version: u32,
    kind: String,
    generated_at_ms: u64,
    application_version: String,
    analysis_version: String,
    sources: Vec<ExportSource>,
    ranges: Vec<ExportRange>,
    calibration_ids: Vec<String>,
    redaction: String,
    analysis: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportRange {
    from_ms: u64,
    to_ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn source(path: &Path) -> ExportSource {
    let f = index::Fingerprint::of(path);
    ExportSource {
        session_id: path
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string()),
        path: path.to_string_lossy().to_string(),
        bytes: f.as_ref().map(|f| f.bytes).unwrap_or(0),
        mtime_s: f.as_ref().map(|f| f.mtime_s).unwrap_or(0),
    }
}

fn active_calibration_ids(sessions_dir: &Path) -> Vec<String> {
    calibration::active_id(sessions_dir, calibration::TARGET_DISPLAY_POWER)
        .into_iter()
        .collect()
}

/// Write a tidy CSV for one session to the report directory. Refuses to
/// overwrite unless `force`; the name is sanitized and confined.
pub fn export_tidy_csv(
    sessions_dir: &Path,
    requested: &str,
    redact: bool,
    force: bool,
) -> Result<String, BridgeError> {
    let target = crate::resolve_session_path(sessions_dir, requested)?;
    let s = sessions::load_full(&target)?;
    let session_id = target
        .file_name()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();
    let settings = settings::load(sessions_dir);
    let dir = settings::report_dir(sessions_dir, &settings)?;
    let name = format!("{}.tidy.csv", sanitize_file_stem(&session_id));
    let path = dir.join(&name);
    let handle = if force {
        std::fs::File::create(&path)
    } else {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
    };
    let handle = handle.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            BridgeError::new(
                "already-exists",
                format!("{name} already exists; choose overwrite to replace it"),
            )
        } else {
            BridgeError::new("io-error", "cannot write export").with_detail(e.to_string())
        }
    })?;
    let mut w = BufWriter::new(handle);
    pf_core::export::write_tidy_csv(&mut w, &s, &session_id, redact).map_err(|e| {
        BridgeError::new("io-error", "cannot write export").with_detail(e.to_string())
    })?;
    Ok(path.to_string_lossy().to_string())
}

/// Build the versioned JSON envelope for a report/export request and write it
/// to the report directory. Returns the written path.
pub fn export_analysis_json(
    sessions_dir: &Path,
    req: &ReportRequest,
) -> Result<String, BridgeError> {
    let envelope = build_envelope(sessions_dir, req)?;
    let settings = settings::load(sessions_dir);
    let dir = settings::report_dir(sessions_dir, &settings)?;
    let stem = export_stem(req);
    let path = dir.join(format!("{stem}.json"));
    let text = serde_json::to_string_pretty(&envelope).map_err(|e| {
        BridgeError::new("internal", "cannot serialize export").with_detail(e.to_string())
    })?;
    std::fs::write(&path, text).map_err(|e| {
        BridgeError::new("io-error", "cannot write export").with_detail(e.to_string())
    })?;
    Ok(path.to_string_lossy().to_string())
}

fn export_stem(req: &ReportRequest) -> String {
    match req.kind.as_str() {
        "analyze" => format!(
            "{}-analyze",
            sanitize_file_stem(req.path.as_deref().unwrap_or("session"))
        ),
        "compare" => format!(
            "{}-vs-{}-compare",
            sanitize_file_stem(req.path.as_deref().unwrap_or("a")),
            sanitize_file_stem(req.path_b.as_deref().unwrap_or("b"))
        ),
        "experiment" => format!(
            "{}-experiment",
            sanitize_file_stem(req.id.as_deref().unwrap_or("experiment"))
        ),
        "calibration" => format!(
            "{}-calibration",
            sanitize_file_stem(req.id.as_deref().unwrap_or("calibration"))
        ),
        other => sanitize_file_stem(other),
    }
}

fn build_envelope(sessions_dir: &Path, req: &ReportRequest) -> Result<ExportEnvelope, BridgeError> {
    let redaction = if req.options.redact {
        "redacted"
    } else {
        "original"
    };
    match req.kind.as_str() {
        "analyze" => {
            let requested = req
                .path
                .as_deref()
                .ok_or_else(|| BridgeError::new("bad-request", "analyze export needs a path"))?;
            let target = crate::resolve_session_path(sessions_dir, requested)?;
            let caps = capabilities::current_report(elevation::is_elevated())?;
            let overview = analyze::overview(&target, &caps)?;
            let from = req.from_ms.unwrap_or(overview.start_wall_ms);
            let to = req.to_ms.unwrap_or(overview.end_wall_ms.max(from));
            let ra = analyze::range_analysis(&target, overview.interval_ms.max(1), from, to)?;
            Ok(ExportEnvelope {
                schema_version: EXPORT_SCHEMA_VERSION,
                kind: "analyze".to_string(),
                generated_at_ms: now_ms(),
                application_version: env!("CARGO_PKG_VERSION").to_string(),
                analysis_version: "analyze-1".to_string(),
                sources: vec![source(&target)],
                ranges: vec![ExportRange {
                    from_ms: ra.from_ms,
                    to_ms: ra.to_ms,
                }],
                calibration_ids: active_calibration_ids(sessions_dir),
                redaction: redaction.to_string(),
                analysis: serde_json::to_value(ra)
                    .map_err(|e| BridgeError::new("internal", e.to_string()))?,
            })
        }
        "compare" => {
            let a = req
                .path
                .as_deref()
                .ok_or_else(|| BridgeError::new("bad-request", "compare export needs path A"))?;
            let b = req
                .path_b
                .as_deref()
                .ok_or_else(|| BridgeError::new("bad-request", "compare export needs path B"))?;
            let a = crate::resolve_session_path(sessions_dir, a)?;
            let b = crate::resolve_session_path(sessions_dir, b)?;
            let c = compare::compare(&a, req.from_ms, req.to_ms, &b, req.from_ms_b, req.to_ms_b)?;
            Ok(ExportEnvelope {
                schema_version: EXPORT_SCHEMA_VERSION,
                kind: "compare".to_string(),
                generated_at_ms: now_ms(),
                application_version: env!("CARGO_PKG_VERSION").to_string(),
                analysis_version: "compare-1".to_string(),
                sources: vec![source(&a), source(&b)],
                ranges: vec![
                    ExportRange {
                        from_ms: c.a.from_ms,
                        to_ms: c.a.to_ms,
                    },
                    ExportRange {
                        from_ms: c.b.from_ms,
                        to_ms: c.b.to_ms,
                    },
                ],
                calibration_ids: active_calibration_ids(sessions_dir),
                redaction: redaction.to_string(),
                analysis: serde_json::to_value(c)
                    .map_err(|e| BridgeError::new("internal", e.to_string()))?,
            })
        }
        "experiment" => {
            let id = req
                .id
                .as_deref()
                .ok_or_else(|| BridgeError::new("bad-request", "experiment export needs an id"))?;
            let record = experiment::get(sessions_dir, id)?;
            let analysis = experiment::analyze(sessions_dir, id)?;
            let mut sources = Vec::new();
            let mut ranges = Vec::new();
            for run in &record.runs {
                if let Ok(p) = crate::resolve_session_path(sessions_dir, &run.session_path)
                    && index::Fingerprint::of(&p).is_some()
                {
                    sources.push(source(&p));
                }
                if run.from_ms.is_some() || run.to_ms.is_some() {
                    ranges.push(ExportRange {
                        from_ms: run.from_ms.unwrap_or(0),
                        to_ms: run.to_ms.unwrap_or(0),
                    });
                }
            }
            Ok(ExportEnvelope {
                schema_version: EXPORT_SCHEMA_VERSION,
                kind: "experiment".to_string(),
                generated_at_ms: now_ms(),
                application_version: env!("CARGO_PKG_VERSION").to_string(),
                analysis_version: format!("experiment-{}", analysis.analysis_version),
                sources,
                ranges,
                calibration_ids: active_calibration_ids(sessions_dir),
                redaction: redaction.to_string(),
                analysis: serde_json::to_value(analysis)
                    .map_err(|e| BridgeError::new("internal", e.to_string()))?,
            })
        }
        "calibration" => {
            let id = req
                .id
                .as_deref()
                .ok_or_else(|| BridgeError::new("bad-request", "calibration export needs an id"))?;
            let rec = calibration::get(sessions_dir, id)?;
            let mut sources = Vec::new();
            for d in &rec.dataset {
                if let Ok(p) = crate::resolve_session_path(sessions_dir, &d.session_id)
                    && index::Fingerprint::of(&p).is_some()
                {
                    sources.push(source(&p));
                }
            }
            Ok(ExportEnvelope {
                schema_version: EXPORT_SCHEMA_VERSION,
                kind: "calibration".to_string(),
                generated_at_ms: now_ms(),
                application_version: env!("CARGO_PKG_VERSION").to_string(),
                analysis_version: "calibration-1".to_string(),
                sources,
                ranges: rec
                    .dataset
                    .iter()
                    .filter(|d| d.from_ms.is_some() || d.to_ms.is_some())
                    .map(|d| ExportRange {
                        from_ms: d.from_ms.unwrap_or(0),
                        to_ms: d.to_ms.unwrap_or(0),
                    })
                    .collect(),
                calibration_ids: vec![rec.id.clone()],
                redaction: redaction.to_string(),
                analysis: serde_json::to_value(rec)
                    .map_err(|e| BridgeError::new("internal", e.to_string()))?,
            })
        }
        other => Err(BridgeError::new(
            "bad-request",
            format!("unknown export kind '{other}'"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_stems_are_sanitized() {
        let req = ReportRequest {
            kind: "analyze".to_string(),
            path: Some("..\\..\\evil".to_string()),
            from_ms: None,
            to_ms: None,
            path_b: None,
            from_ms_b: None,
            to_ms_b: None,
            id: None,
            options: Default::default(),
        };
        let stem = export_stem(&req);
        assert!(!stem.contains('\\') && !stem.contains('/'), "{stem}");
    }

    #[test]
    fn tidy_csv_export_preserves_absence_and_writes_to_report_dir() {
        let dir =
            std::env::temp_dir().join(format!("pf-gui-export-{}-{}", std::process::id(), now_ms()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut lines = String::new();
        lines.push_str(
            "{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000,\"note\":\"x\"}\n",
        );
        lines.push_str("{\"collector\":\"battery\",\"wall_ms\":1000,\"mono_ms\":0,\"discharge_w\":{\"v\":6.2,\"p\":\"measured\"},\"charge_w\":{\"v\":null,\"p\":\"unavailable\",\"k\":2,\"q\":4,\"reason\":\"not charging\"},\"charge_pct\":{\"v\":71,\"p\":\"measured\"}}\n");
        lines.push_str("{\"type\":\"session_footer\",\"wall_ms\":2000,\"summary\":{\"samples\":1,\"discharge_median_w\":6.2,\"discharge_coverage_pct\":90.0,\"recovered\":false}}\n");
        std::fs::write(dir.join("s.jsonl"), lines).unwrap();
        let path = export_tidy_csv(&dir, "s.jsonl", false, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(pf_core::export::TIDY_CSV_HEADER), "{text}");
        assert!(text.contains("unavailable_kind"), "{text}");
        assert!(text.contains("not charging"), "{text}");
        // The unavailable charge row has an empty value cell, never a zero.
        let charge = text.lines().find(|l| l.contains(",charge_w,")).unwrap();
        assert!(charge.contains(",charge_w,,,W,unavailable,"), "{charge}");
        // Refusing to overwrite unless forced.
        assert!(export_tidy_csv(&dir, "s.jsonl", false, false).is_err());
        assert!(export_tidy_csv(&dir, "s.jsonl", false, true).is_ok());
        // Missing source is a typed not-found error.
        let err = export_tidy_csv(&dir, "missing.jsonl", false, true).unwrap_err();
        assert_eq!(err.code, "not-found");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn calibration_json_export_is_versioned_and_keeps_estimated_class() {
        let dir = std::env::temp_dir().join(format!(
            "pf-gui-export-cal-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let request = crate::calibration::CreateCalibrationRequest {
            target: crate::calibration::TARGET_DISPLAY_POWER.to_string(),
            reference_kind: "external_meter".to_string(),
            references: vec![
                crate::calibration::CalibrationReferenceDto {
                    input: 0.0,
                    reference: 5.0,
                    kind: "external_meter".to_string(),
                    source_label: "bench".to_string(),
                    note: String::new(),
                },
                crate::calibration::CalibrationReferenceDto {
                    input: 100.0,
                    reference: 10.0,
                    kind: "external_meter".to_string(),
                    source_label: "bench".to_string(),
                    note: String::new(),
                },
            ],
            dataset: vec![],
            exclusions: vec![],
            notes: String::new(),
            machine: None,
            panel: None,
            power_source: None,
            activate: true,
        };
        let rec = crate::calibration::create(&dir, request).unwrap();
        let req = ReportRequest {
            kind: "calibration".to_string(),
            path: None,
            from_ms: None,
            to_ms: None,
            path_b: None,
            from_ms_b: None,
            to_ms_b: None,
            id: Some(rec.id.clone()),
            options: Default::default(),
        };
        let path = export_analysis_json(&dir, &req).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"schemaVersion\": 1"), "{text}");
        assert!(text.contains("\"kind\": \"calibration\""), "{text}");
        assert!(text.contains(&rec.id), "{text}");
        // The export can never promote the estimate to a measurement.
        assert!(text.contains("\"evidenceClass\": \"estimated\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let req = ReportRequest {
            kind: "nope".to_string(),
            path: None,
            from_ms: None,
            to_ms: None,
            path_b: None,
            from_ms_b: None,
            to_ms_b: None,
            id: None,
            options: Default::default(),
        };
        let dir = std::env::temp_dir();
        assert!(build_envelope(&dir, &req).is_err());
    }
}
