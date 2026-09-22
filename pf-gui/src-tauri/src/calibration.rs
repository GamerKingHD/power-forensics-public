//! GUI calibration workspace backend.
//!
//! Persistence and DTOs live here; the fitting/quality science stays in
//! [`pf_core::calibration`]. A calibration is never treated as a measurement:
//! records carry `evidenceClass = "estimated"` and the model only ever turns
//! brightness into an incremental display-power *estimate*.
//!
//! Revisions are append-only: recalibrating creates a new revision and never
//! overwrites history. Switching the active revision is recorded in an
//! append-only event log so the change is auditable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use pf_core::calibration as core_cal;

use crate::dto::BridgeError;

pub const CALIBRATION_DIR: &str = "calibrations";
pub const TARGET_DISPLAY_POWER: &str = "display_power";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationReferenceDto {
    /// Input level (brightness percent for display power).
    pub input: f64,
    /// Independent reference value (watts).
    pub reference: f64,
    /// "external_meter" | "bench_meter" | "oem_reading" | "manual_reference" |
    /// "fitted_against_estimate"
    pub kind: String,
    pub source_label: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationDatasetRefDto {
    pub session_id: String,
    #[serde(default)]
    pub from_ms: Option<u64>,
    #[serde(default)]
    pub to_ms: Option<u64>,
    /// "raw_evidence" | "reference"
    pub role: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationExclusionDto {
    pub session_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationModelDto {
    pub slope_w_per_pct: f64,
    pub intercept_w: f64,
    pub min_brightness: f64,
    pub r2_fit: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationValidationDto {
    pub n: usize,
    pub mae: f64,
    pub rmse: f64,
    pub median_abs_error: f64,
    pub max_abs_error: f64,
    pub r2: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationResidualDto {
    pub input: f64,
    pub predicted: f64,
    pub reference: f64,
    pub abs_error: f64,
    pub relative_error: Option<f64>,
    pub held_out: bool,
    pub in_domain: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationApplicabilityDto {
    pub machine: String,
    pub panel: String,
    pub input_min: f64,
    pub input_max: f64,
    #[serde(default)]
    pub power_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationRecordDto {
    pub schema_version: u32,
    /// Stable id: `<target>#<revision>`.
    pub id: String,
    pub revision: u32,
    pub created_at_ms: u64,
    pub target: String,
    pub model_kind: String,
    /// Always "estimated": a calibration never promotes an estimate.
    pub evidence_class: String,
    pub reference_kind: String,
    pub independent_reference: bool,
    pub model: CalibrationModelDto,
    pub applicability: CalibrationApplicabilityDto,
    pub references: Vec<CalibrationReferenceDto>,
    pub dataset: Vec<CalibrationDatasetRefDto>,
    pub exclusions: Vec<CalibrationExclusionDto>,
    /// None when the dataset is too small for a held-out split.
    #[serde(default)]
    pub validation: Option<CalibrationValidationDto>,
    pub residuals: Vec<CalibrationResidualDto>,
    /// "validated" | "usable_with_caveats" | "weak" | "insufficient_evidence" |
    /// "out_of_domain"
    pub quality: String,
    pub quality_reasons: Vec<String>,
    #[serde(default)]
    pub notes: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveEventDto {
    pub id: String,
    #[serde(default)]
    pub previous_id: Option<String>,
    pub at_ms: u64,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CalibrationSetFile {
    schema_version: u32,
    target: String,
    active_id: Option<String>,
    revisions: Vec<CalibrationRecordFile>,
    #[serde(default)]
    active_events: Vec<ActiveEventDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CalibrationRecordFile {
    id: String,
    revision: u32,
    created_at_ms: u64,
    target: String,
    model_kind: String,
    evidence_class: String,
    reference_kind: String,
    independent_reference: bool,
    model: CalibrationModelDto,
    applicability: CalibrationApplicabilityDto,
    references: Vec<CalibrationReferenceDto>,
    dataset: Vec<CalibrationDatasetRefDto>,
    exclusions: Vec<CalibrationExclusionDto>,
    validation: Option<CalibrationValidationDto>,
    residuals: Vec<CalibrationResidualDto>,
    quality: String,
    quality_reasons: Vec<String>,
    notes: String,
}

impl CalibrationRecordFile {
    fn to_dto(&self, active: bool) -> CalibrationRecordDto {
        CalibrationRecordDto {
            schema_version: core_cal::CALIBRATION_SCHEMA_VERSION,
            id: self.id.clone(),
            revision: self.revision,
            created_at_ms: self.created_at_ms,
            target: self.target.clone(),
            model_kind: self.model_kind.clone(),
            evidence_class: self.evidence_class.clone(),
            reference_kind: self.reference_kind.clone(),
            independent_reference: self.independent_reference,
            model: self.model.clone(),
            applicability: self.applicability.clone(),
            references: self.references.clone(),
            dataset: self.dataset.clone(),
            exclusions: self.exclusions.clone(),
            validation: self.validation.clone(),
            residuals: self.residuals.clone(),
            quality: self.quality.clone(),
            quality_reasons: self.quality_reasons.clone(),
            notes: self.notes.clone(),
            active,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCalibrationRequest {
    pub target: String,
    pub reference_kind: String,
    pub references: Vec<CalibrationReferenceDto>,
    #[serde(default)]
    pub dataset: Vec<CalibrationDatasetRefDto>,
    #[serde(default)]
    pub exclusions: Vec<CalibrationExclusionDto>,
    #[serde(default)]
    pub notes: String,
    /// Hardware identity; when omitted it is detected from the current machine.
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub panel: Option<String>,
    #[serde(default)]
    pub power_source: Option<String>,
    #[serde(default = "default_true")]
    pub activate: bool,
}

fn default_true() -> bool {
    true
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn calib_dir(sessions_dir: &Path) -> PathBuf {
    sessions_dir.join(CALIBRATION_DIR)
}

fn set_path(sessions_dir: &Path, target: &str) -> PathBuf {
    calib_dir(sessions_dir).join(format!("{target}.json"))
}

fn validate_target(target: &str) -> Result<(), BridgeError> {
    if target != TARGET_DISPLAY_POWER {
        return Err(BridgeError::new(
            "bad-request",
            format!("unsupported calibration target '{target}'"),
        ));
    }
    Ok(())
}

fn target_from_id(id: &str) -> Result<&str, BridgeError> {
    let (target, revision) = id
        .split_once('#')
        .ok_or_else(|| BridgeError::new("bad-request", "invalid calibration id"))?;
    validate_target(target)?;
    if revision.is_empty() || !revision.chars().all(|c| c.is_ascii_digit()) {
        return Err(BridgeError::new(
            "bad-request",
            "invalid calibration revision",
        ));
    }
    Ok(target)
}

/// Current machine + primary-panel identity, matching the CLI calibration
/// gate. Panel is the EDID-style monitor token when it can be enumerated.
pub fn current_display_identity() -> (String, String) {
    let machine = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string());
    let panel = pf_collectors::display::DisplayCollector::new()
        .read()
        .ok()
        .and_then(|s| {
            let d = s
                .displays
                .iter()
                .find(|d| d.primary)
                .or_else(|| s.displays.first())?;
            let token = pf_collectors::display::monitor_token(&d.name);
            Some(if token.is_empty() || token == "." {
                d.name.clone()
            } else {
                token
            })
        })
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    (machine, panel)
}

fn load_set(sessions_dir: &Path, target: &str) -> CalibrationSetFile {
    let path = set_path(sessions_dir, target);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return CalibrationSetFile {
            schema_version: core_cal::CALIBRATION_SCHEMA_VERSION,
            target: target.to_string(),
            ..Default::default()
        };
    };
    serde_json::from_str(&text).unwrap_or(CalibrationSetFile {
        schema_version: core_cal::CALIBRATION_SCHEMA_VERSION,
        target: target.to_string(),
        ..Default::default()
    })
}

fn save_set(sessions_dir: &Path, set: &CalibrationSetFile) -> Result<(), BridgeError> {
    let dir = calib_dir(sessions_dir);
    std::fs::create_dir_all(&dir).map_err(|e| {
        BridgeError::new("io-error", "cannot create calibration directory")
            .with_detail(e.to_string())
    })?;
    let text = serde_json::to_string_pretty(set).map_err(|e| {
        BridgeError::new("internal", "cannot serialize calibration").with_detail(e.to_string())
    })?;
    let target = set_path(sessions_dir, &set.target);
    let tmp = target.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| {
        BridgeError::new("io-error", "cannot write calibration").with_detail(e.to_string())
    })?;
    std::fs::rename(&tmp, &target).map_err(|e| {
        BridgeError::new("io-error", "cannot finalize calibration").with_detail(e.to_string())
    })?;
    Ok(())
}

/// Active calibration id for a target, if any. Used to bind analysis caches to
/// the calibration identity so a cache built under a previous revision is never
/// served after the active revision changes.
pub fn active_id(sessions_dir: &Path, target: &str) -> Option<String> {
    load_set(sessions_dir, target).active_id
}

pub fn list(sessions_dir: &Path) -> Result<Vec<CalibrationRecordDto>, BridgeError> {
    // Only the display-power target is exposed: it is the only relationship the
    // backend can justify with an independent reference.
    let set = load_set(sessions_dir, TARGET_DISPLAY_POWER);
    let mut out: Vec<CalibrationRecordDto> = set
        .revisions
        .iter()
        .map(|r| r.to_dto(set.active_id.as_deref() == Some(r.id.as_str())))
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.revision));
    Ok(out)
}

pub fn get(sessions_dir: &Path, id: &str) -> Result<CalibrationRecordDto, BridgeError> {
    let target = target_from_id(id)?;
    let set = load_set(sessions_dir, target);
    set.revisions
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.to_dto(set.active_id.as_deref() == Some(id)))
        .ok_or_else(|| BridgeError::new("not-found", format!("unknown calibration: {id}")))
}

/// Fit and persist a new calibration revision. Returns the created record.
pub fn create(
    sessions_dir: &Path,
    request: CreateCalibrationRequest,
) -> Result<CalibrationRecordDto, BridgeError> {
    let target = core_cal::CalibrationTarget::parse(&request.target).ok_or_else(|| {
        BridgeError::new(
            "bad-request",
            format!(
                "unsupported calibration target '{}': only display_power is calibratable",
                request.target
            ),
        )
    })?;
    let reference_kind =
        core_cal::ReferenceKind::parse(&request.reference_kind).ok_or_else(|| {
            BridgeError::new(
                "bad-request",
                format!("unknown reference kind '{}'", request.reference_kind),
            )
        })?;
    if request.references.len() < 2 {
        return Err(BridgeError::new(
            "insufficient-evidence",
            "calibration needs at least two reference points",
        ));
    }
    let points: Vec<(f64, f64)> = request
        .references
        .iter()
        .map(|r| (r.input, r.reference))
        .collect();
    let fit = core_cal::fit_display_calibration(&points).ok_or_else(|| {
        BridgeError::new(
            "insufficient-evidence",
            "reference points do not span a fit: need distinct input levels",
        )
    })?;

    let (detected_machine, detected_panel) = current_display_identity();
    let machine = request
        .machine
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| detected_machine.clone());
    let panel = request
        .panel
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| detected_panel.clone());
    let reference_span = points
        .iter()
        .map(|(_, y)| *y)
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), y| {
            (lo.min(y), hi.max(y))
        });
    let reference_span = Some((reference_span.1 - reference_span.0).abs());
    let hardware_matches = machine == detected_machine && panel == detected_panel;

    let (quality, reasons) = core_cal::classify(&core_cal::QualityInput {
        n: fit.n,
        independent_validation: fit.independent_validation,
        validation: fit.validation.clone(),
        reference_span,
        input_min: fit.input_min,
        input_max: fit.input_max,
        target_domain: target.input_domain(),
        independent_reference: reference_kind.is_independent(),
        hardware_matches,
    });

    let model = core_cal::display_model_from_fit(&fit, &machine, &panel, now_ms(), points.clone());

    let residuals = fit
        .residuals
        .iter()
        .map(|r| CalibrationResidualDto {
            input: r.input,
            predicted: r.predicted,
            reference: r.reference,
            abs_error: r.abs_error,
            relative_error: r.relative_error,
            held_out: r.held_out,
            in_domain: r.in_domain,
        })
        .collect();

    let mut set = load_set(sessions_dir, &request.target);
    set.target = request.target.clone();
    set.schema_version = core_cal::CALIBRATION_SCHEMA_VERSION;
    let revision = set.revisions.iter().map(|r| r.revision).max().unwrap_or(0) + 1;
    let id = format!("{}#{revision}", request.target);
    let record = CalibrationRecordFile {
        id: id.clone(),
        revision,
        created_at_ms: now_ms(),
        target: request.target.clone(),
        model_kind: "display_linear".to_string(),
        evidence_class: "estimated".to_string(),
        reference_kind: request.reference_kind.clone(),
        independent_reference: reference_kind.is_independent(),
        model: CalibrationModelDto {
            slope_w_per_pct: model.slope_w_per_pct,
            intercept_w: model.intercept_w,
            min_brightness: model.min_brightness,
            r2_fit: model.r2,
        },
        applicability: CalibrationApplicabilityDto {
            machine,
            panel,
            input_min: fit.input_min,
            input_max: fit.input_max,
            power_source: request.power_source.clone(),
        },
        references: request.references.clone(),
        dataset: request.dataset.clone(),
        exclusions: request.exclusions.clone(),
        validation: fit.validation.as_ref().map(|v| CalibrationValidationDto {
            n: v.n,
            mae: v.mae,
            rmse: v.rmse,
            median_abs_error: v.median_abs_error,
            max_abs_error: v.max_abs_error,
            r2: v.r2,
        }),
        residuals,
        quality: quality.as_str().to_string(),
        quality_reasons: reasons,
        notes: request.notes.clone(),
    };
    set.revisions.push(record.clone());
    if request.activate {
        set.active_events.push(ActiveEventDto {
            id: id.clone(),
            previous_id: set.active_id.clone(),
            at_ms: now_ms(),
            action: "activated".to_string(),
        });
        set.active_id = Some(id.clone());
    }
    save_set(sessions_dir, &set)?;
    get(sessions_dir, &id)
}

/// Switch the active revision. Auditable via the append-only event log.
pub fn set_active(sessions_dir: &Path, id: &str) -> Result<CalibrationRecordDto, BridgeError> {
    let target = target_from_id(id)?;
    let mut set = load_set(sessions_dir, target);
    if !set.revisions.iter().any(|r| r.id == id) {
        return Err(BridgeError::new(
            "not-found",
            format!("unknown calibration: {id}"),
        ));
    }
    set.active_events.push(ActiveEventDto {
        id: id.to_string(),
        previous_id: set.active_id.clone(),
        at_ms: now_ms(),
        action: "activated".to_string(),
    });
    set.active_id = Some(id.to_string());
    save_set(sessions_dir, &set)?;
    get(sessions_dir, id)
}

/// Audit trail of active-calibration switches for a target.
pub fn active_history(
    sessions_dir: &Path,
    target: &str,
) -> Result<Vec<ActiveEventDto>, BridgeError> {
    validate_target(target)?;
    Ok(load_set(sessions_dir, target).active_events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pf-gui-calib-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn req(references: Vec<(f64, f64)>, activate: bool) -> CreateCalibrationRequest {
        CreateCalibrationRequest {
            target: TARGET_DISPLAY_POWER.to_string(),
            reference_kind: "external_meter".to_string(),
            references: references
                .into_iter()
                .map(|(input, reference)| CalibrationReferenceDto {
                    input,
                    reference,
                    kind: "external_meter".to_string(),
                    source_label: "bench".to_string(),
                    note: String::new(),
                })
                .collect(),
            dataset: vec![CalibrationDatasetRefDto {
                session_id: "s1.jsonl".to_string(),
                from_ms: Some(0),
                to_ms: Some(1000),
                role: "raw_evidence".to_string(),
                note: String::new(),
            }],
            exclusions: vec![],
            notes: "test".to_string(),
            // Default to the detected hardware identity so quality reflects the
            // evidence, not a deliberate mismatch.
            machine: None,
            panel: None,
            power_source: Some("battery".to_string()),
            activate,
        }
    }

    #[test]
    fn rejects_untrusted_calibration_targets_and_ids() {
        let dir = temp_dir("target-confinement");
        assert!(get(&dir, "..\\outside#1").is_err());
        assert!(get(&dir, "C:\\outside#1").is_err());
        assert!(set_active(&dir, "..\\outside#1").is_err());
        assert!(active_history(&dir, "..\\outside").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_builds_revision_and_marks_evidence_estimated() {
        let dir = temp_dir("create");
        let rec = create(
            &dir,
            req(
                vec![
                    (0.0, 5.0),
                    (20.0, 6.0),
                    (40.0, 7.0),
                    (60.0, 8.0),
                    (80.0, 9.0),
                    (100.0, 10.0),
                ],
                true,
            ),
        )
        .unwrap();
        assert_eq!(rec.revision, 1);
        assert_eq!(rec.id, "display_power#1");
        assert_eq!(rec.evidence_class, "estimated");
        assert!(rec.active);
        assert!((rec.model.slope_w_per_pct - 0.05).abs() < 1e-9);
        assert!(rec.validation.is_some());
        assert_eq!(rec.quality, "validated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recalibration_creates_a_new_revision_and_keeps_history() {
        let dir = temp_dir("rev");
        let r1 = create(
            &dir,
            req(vec![(0.0, 5.0), (50.0, 7.5), (100.0, 10.0)], true),
        )
        .unwrap();
        let r2 = create(
            &dir,
            req(vec![(0.0, 5.1), (50.0, 7.4), (100.0, 10.2)], true),
        )
        .unwrap();
        assert_eq!(r1.revision, 1);
        assert_eq!(r2.revision, 2);
        let all = list(&dir).unwrap();
        assert_eq!(all.len(), 2, "history is append-only");
        assert!(all.iter().find(|r| r.id == r1.id).is_some());
        assert_eq!(all.iter().find(|r| r.active).unwrap().id, r2.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn switching_active_is_recorded_in_history() {
        let dir = temp_dir("active");
        let r1 = create(
            &dir,
            req(vec![(0.0, 5.0), (50.0, 7.5), (100.0, 10.0)], false),
        )
        .unwrap();
        let r2 = create(
            &dir,
            req(vec![(0.0, 5.1), (50.0, 7.4), (100.0, 10.2)], true),
        )
        .unwrap();
        set_active(&dir, &r1.id).unwrap();
        assert_eq!(
            active_id(&dir, TARGET_DISPLAY_POWER).as_deref(),
            Some(r1.id.as_str())
        );
        let history = active_history(&dir, TARGET_DISPLAY_POWER).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history.last().unwrap().id, r1.id);
        assert_eq!(
            history.last().unwrap().previous_id.as_deref(),
            Some(r2.id.as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn insufficient_reference_points_are_refused() {
        let dir = temp_dir("insufficient");
        let err = create(&dir, req(vec![(50.0, 8.0)], true)).unwrap_err();
        assert_eq!(err.code, "insufficient-evidence");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn estimate_only_reference_caps_quality() {
        let dir = temp_dir("estimate-only");
        let mut request = req(
            vec![
                (0.0, 5.0),
                (20.0, 6.0),
                (40.0, 7.0),
                (60.0, 8.0),
                (80.0, 9.0),
                (100.0, 10.0),
            ],
            true,
        );
        request.reference_kind = "fitted_against_estimate".to_string();
        for r in request.references.iter_mut() {
            r.kind = "fitted_against_estimate".to_string();
        }
        let rec = create(&dir, request).unwrap();
        assert!(!rec.independent_reference);
        assert_ne!(rec.quality, "validated");
        assert!(
            rec.quality_reasons
                .iter()
                .any(|r| r.contains("another estimate"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hardware_mismatch_caps_quality_at_weak() {
        let dir = temp_dir("hw-mismatch");
        let mut request = req(
            vec![
                (0.0, 5.0),
                (20.0, 6.0),
                (40.0, 7.0),
                (60.0, 8.0),
                (80.0, 9.0),
                (100.0, 10.0),
            ],
            true,
        );
        request.machine = Some("SOME-OTHER-MACHINE".to_string());
        request.panel = Some("SOME-OTHER-PANEL".to_string());
        let rec = create(&dir, request).unwrap();
        assert_ne!(rec.quality, "validated");
        assert!(
            rec.quality_reasons
                .iter()
                .any(|r| r.contains("hardware identity"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupported_target_is_refused() {
        let dir = temp_dir("target");
        let mut request = req(vec![(0.0, 5.0), (50.0, 7.5)], true);
        request.target = "cpu_power".to_string();
        let err = create(&dir, request).unwrap_err();
        assert_eq!(err.code, "bad-request");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_calibration_store_is_ignored_not_fatal() {
        let dir = temp_dir("corrupt");
        let path = set_path(&dir, TARGET_DISPLAY_POWER);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not json").unwrap();
        // A corrupt store must not panic or surface an error: it degrades to
        // "no calibration", so analysis simply does not use one.
        assert!(list(&dir).unwrap().is_empty());
        assert_eq!(active_id(&dir, TARGET_DISPLAY_POWER), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
