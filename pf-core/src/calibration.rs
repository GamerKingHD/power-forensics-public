//! Calibration science: fitting, validation and quality classification.
//!
//! Calibration NEVER promotes an estimate to a measurement. A calibrated
//! display-power value stays `Estimated` (or `Derived`) in the evidence model;
//! what calibration adds is a fitted, versioned, auditable relationship plus an
//! honest statement of how well it predicts an independent reference.
//!
//! This module is pure and dependency-free on purpose: persistence (revisions,
//! active selection) and DTOs live in the desktop bridge, while the fitting and
//! quality rules live here and are tested directly.
//!
//! Units: input (brightness) in percent, reference (power) in watts.

use crate::calib::{self, DisplayModel};

/// Schema version of the persisted calibration domain. Bump on shape change.
pub const CALIBRATION_SCHEMA_VERSION: u32 = 1;

/// The metric a calibration produces. Only targets the backend can actually
/// justify are exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationTarget {
    /// Incremental display power versus brightness, calibrated against an
    /// independent power reference.
    DisplayPower,
}

impl CalibrationTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            CalibrationTarget::DisplayPower => "display_power",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "display_power" => Some(CalibrationTarget::DisplayPower),
            _ => None,
        }
    }

    /// Input range the target is defined over (brightness percent).
    pub fn input_domain(self) -> (f64, f64) {
        match self {
            CalibrationTarget::DisplayPower => (0.0, 100.0),
        }
    }
}

/// What serves as the reference (ground truth) for a calibration.
///
/// Every variant except [`ReferenceKind::FittedAgainstEstimate`] is an
/// independent, external/user-provided measurement. A model fitted only
/// against another estimate must say so and is never called validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceKind {
    /// Reading from an external wall/mains power meter.
    ExternalMeter,
    /// Reading from a bench/lab power supply meter.
    BenchMeter,
    /// Vendor/OEM-provided figure the user enters.
    OemReading,
    /// Any other user-supplied reference value.
    ManualReference,
    /// No independent reference: fitted against another estimate. Explicit.
    FittedAgainstEstimate,
}

impl ReferenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ReferenceKind::ExternalMeter => "external_meter",
            ReferenceKind::BenchMeter => "bench_meter",
            ReferenceKind::OemReading => "oem_reading",
            ReferenceKind::ManualReference => "manual_reference",
            ReferenceKind::FittedAgainstEstimate => "fitted_against_estimate",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "external_meter" => ReferenceKind::ExternalMeter,
            "bench_meter" => ReferenceKind::BenchMeter,
            "oem_reading" => ReferenceKind::OemReading,
            "manual_reference" => ReferenceKind::ManualReference,
            "fitted_against_estimate" => ReferenceKind::FittedAgainstEstimate,
            _ => return None,
        })
    }

    /// Whether the reference is independent of the estimated quantity. A fit
    /// against another estimate is not independent evidence.
    pub fn is_independent(self) -> bool {
        !matches!(self, ReferenceKind::FittedAgainstEstimate)
    }

    /// Human label. Deliberately never says "measured by power-forensics".
    pub fn label(self) -> &'static str {
        match self {
            ReferenceKind::ExternalMeter => "external meter (user-provided)",
            ReferenceKind::BenchMeter => "bench meter (user-provided)",
            ReferenceKind::OemReading => "OEM reading (user-provided)",
            ReferenceKind::ManualReference => "manual reference (user-provided)",
            ReferenceKind::FittedAgainstEstimate => "fitted against another estimate",
        }
    }
}

/// One (input, reference) pair supplied to the fit.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferencePoint {
    /// Input level (brightness percent for display power).
    pub input: f64,
    /// Independent reference value (watts).
    pub reference: f64,
    pub kind: ReferenceKind,
    /// Short source label, e.g. "Kill-A-Watt", "bench PSU".
    pub source_label: String,
    pub note: String,
}

/// Robust summary of held-out prediction error. Error is in reference units.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationStats {
    pub n: usize,
    pub mae: f64,
    pub rmse: f64,
    pub median_abs_error: f64,
    pub max_abs_error: f64,
    /// R² on held-out points; `None` for a degenerate reference spread.
    pub r2: Option<f64>,
}

/// One fit residual, keeping the point's domain status explicit.
#[derive(Debug, Clone, PartialEq)]
pub struct Residual {
    pub input: f64,
    pub predicted: f64,
    pub reference: f64,
    pub abs_error: f64,
    /// `None` when the reference is zero/near-zero: relative error is not
    /// meaningful and is never fabricated.
    pub relative_error: Option<f64>,
    /// True when the point was held out of the training fit.
    pub held_out: bool,
    /// True when the input lies inside the fitted input range.
    pub in_domain: bool,
}

/// Complete result of fitting one line y = slope*x + intercept.
#[derive(Debug, Clone, PartialEq)]
pub struct FitOutcome {
    pub n: usize,
    pub train_n: usize,
    pub validation_n: usize,
    pub slope: f64,
    pub intercept: f64,
    /// Fit quality on every point (training fit). NOT validation quality.
    pub r2_fit: f64,
    /// Independent held-out error, when the dataset allows a split.
    pub validation: Option<ValidationStats>,
    pub residuals: Vec<Residual>,
    pub input_min: f64,
    pub input_max: f64,
    /// True when a train/validation split was possible at all.
    pub independent_validation: bool,
}

/// Minimum points for a held-out split. Below this we fit the line but report
/// that independent validation is unavailable rather than faking precision.
pub const MIN_VALIDATION_N: usize = 4;
/// Fraction of points (rounded) reserved for validation when splitting.
pub const VALIDATION_FRACTION: f64 = 0.3;
/// Below this |reference| relative error is suppressed (division by ~zero).
pub const RELATIVE_EPSILON: f64 = 1e-9;

/// Input/reference point set used by the fit.
type Points = Vec<(f64, f64)>;

/// Fit a display-power calibration. Returns `None` only when a line cannot be
/// fitted at all (fewer than two points or a degenerate input spread).
///
/// The persisted model is fit on all points, but the reported validation
/// statistics come from held-out points, so training fit is never presented as
/// validation quality. With fewer than [`MIN_VALIDATION_N`] points, validation
/// is explicitly `None`.
pub fn fit_display_calibration(points: &[(f64, f64)]) -> Option<FitOutcome> {
    if points.len() < 2 {
        return None;
    }
    let mut sorted: Vec<(f64, f64)> = points.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0));

    let n = sorted.len();
    let independent_validation = n >= MIN_VALIDATION_N;
    let (train, validation): (Points, Points) = if independent_validation {
        // Deterministic hold-out: reserve every k-th (1-based) point, starting
        // from index 1, which spreads validation across the input range.
        let k = (1.0 / VALIDATION_FRACTION).round().max(2.0) as usize;
        let mut train = Vec::new();
        let mut val = Vec::new();
        for (i, p) in sorted.iter().enumerate() {
            if (i + 1) % k == 0 {
                val.push(*p);
            } else {
                train.push(*p);
            }
        }
        // Need at least 2 training points and 1 validation point to be useful.
        if train.len() < 2 || val.is_empty() {
            (sorted.clone(), Vec::new())
        } else {
            (train, val)
        }
    } else {
        (sorted.clone(), Vec::new())
    };

    let fit_all = calib::fit_linear(&sorted)?;
    let input_min = sorted.iter().map(|(x, _)| *x).fold(f64::INFINITY, f64::min);
    let input_max = sorted
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::NEG_INFINITY, f64::max);

    // Persisted model is fit on all points unless the hold-out consumed too
    // much; train fit is used for the validation residuals either way.
    let train_fit = if validation.is_empty() {
        fit_all
    } else {
        calib::fit_linear(&train).unwrap_or(fit_all)
    };

    let mut residuals = Vec::with_capacity(n);
    let mut val_errors: Vec<(f64, f64)> = Vec::new();
    for (x, y) in &sorted {
        let predicted = train_fit.intercept + train_fit.slope * *x;
        let abs_error = (predicted - *y).abs();
        let held_out = validation.iter().any(|(vx, vy)| vx == x && vy == y);
        if held_out {
            val_errors.push((predicted, *y));
        }
        residuals.push(Residual {
            input: *x,
            predicted,
            reference: *y,
            abs_error,
            relative_error: relative_error(predicted, *y),
            held_out,
            in_domain: true,
        });
    }

    let validation_stats = if val_errors.is_empty() {
        None
    } else {
        Some(validation_stats(&val_errors, validation.len()))
    };

    let independent_validation = validation_stats.is_some();
    Some(FitOutcome {
        n,
        train_n: train.len(),
        validation_n: validation.len(),
        slope: fit_all.slope,
        intercept: fit_all.intercept,
        r2_fit: fit_all.r2,
        validation: validation_stats,
        residuals,
        input_min,
        input_max,
        independent_validation,
    })
}

fn relative_error(predicted: f64, reference: f64) -> Option<f64> {
    if reference.abs() < RELATIVE_EPSILON {
        None
    } else {
        Some((predicted - reference) / reference)
    }
}

fn validation_stats(errors: &[(f64, f64)], n: usize) -> ValidationStats {
    let k = errors.len() as f64;
    let mae = errors.iter().map(|(p, r)| (p - r).abs()).sum::<f64>() / k;
    let rmse = (errors.iter().map(|(p, r)| (p - r).powi(2)).sum::<f64>() / k).sqrt();
    let max_abs_error = errors
        .iter()
        .map(|(p, r)| (p - r).abs())
        .fold(0.0_f64, f64::max);
    let mut abs: Vec<f64> = errors.iter().map(|(p, r)| (p - r).abs()).collect();
    abs.sort_by(f64::total_cmp);
    let median_abs_error = crate::stats::median(&abs).unwrap_or(0.0);
    let refs: Vec<f64> = errors.iter().map(|(_, r)| *r).collect();
    let mean_ref = refs.iter().sum::<f64>() / k;
    let ss_tot: f64 = refs.iter().map(|r| (r - mean_ref).powi(2)).sum();
    let ss_res: f64 = errors.iter().map(|(p, r)| (p - r).powi(2)).sum();
    let r2 = if ss_tot.abs() < RELATIVE_EPSILON {
        None
    } else {
        Some((1.0 - ss_res / ss_tot).clamp(-1.0, 1.0))
    };
    ValidationStats {
        n,
        mae,
        rmse,
        median_abs_error,
        max_abs_error,
        r2,
    }
}

/// Structured calibration quality. Derived from multiple evidence dimensions,
/// not a single arbitrary R² threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationQuality {
    /// Independent reference plus a held-out error that is small relative to
    /// the reference span and good input-range coverage.
    Validated,
    /// Usable, but with explicit caveats (few points, no independent
    /// validation, estimate-only reference, modest error).
    UsableWithCaveats,
    /// Fit exists but errors/coverage are poor: treat as weak.
    Weak,
    /// Too little evidence to say anything useful.
    InsufficientEvidence,
    /// Fit only holds over its fitted input range; the requested input is
    /// outside it.
    OutOfDomain,
}

impl CalibrationQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            CalibrationQuality::Validated => "validated",
            CalibrationQuality::UsableWithCaveats => "usable_with_caveats",
            CalibrationQuality::Weak => "weak",
            CalibrationQuality::InsufficientEvidence => "insufficient_evidence",
            CalibrationQuality::OutOfDomain => "out_of_domain",
        }
    }
}

/// Evidence dimensions feeding [`classify`]. Kept explicit so the decision is
/// inspectable and testable.
#[derive(Debug, Clone)]
pub struct QualityInput {
    pub n: usize,
    pub independent_validation: bool,
    /// Held-out error, when available.
    pub validation: Option<ValidationStats>,
    /// Reference spread (max - min) used to normalize error.
    pub reference_span: Option<f64>,
    pub input_min: f64,
    pub input_max: f64,
    pub target_domain: (f64, f64),
    /// Independent (non-estimate) reference present.
    pub independent_reference: bool,
    /// Stored hardware identity matches the machine the calibration is for.
    pub hardware_matches: bool,
}

/// Classify quality from the evidence, returning the class and the reasons
/// that drove it (surfaced to the user verbatim).
pub fn classify(input: &QualityInput) -> (CalibrationQuality, Vec<String>) {
    let mut reasons = Vec::new();
    if !input.hardware_matches {
        reasons.push("hardware identity does not match the current machine".to_string());
    }
    if input.n < 2 {
        reasons.push("fewer than two reference points".to_string());
        return (CalibrationQuality::InsufficientEvidence, reasons);
    }
    if !input.independent_reference {
        reasons.push("reference is another estimate, not an independent measurement".to_string());
    }

    // Range coverage relative to the target domain; a sweep confined to a
    // narrow band cannot support claims across the full range.
    let (dmin, dmax) = input.target_domain;
    let domain_span = (dmax - dmin).max(RELATIVE_EPSILON);
    let covered = (input.input_max - input.input_min).max(0.0);
    let coverage = (covered / domain_span).clamp(0.0, 1.0);
    if coverage < 0.25 {
        reasons.push(format!(
            "input sweep covers only {:.0}% of the target range",
            coverage * 100.0
        ));
    }

    let span = input.reference_span.unwrap_or(0.0).abs();
    let relative_mae = match (&input.validation, span > RELATIVE_EPSILON) {
        (Some(v), true) => Some(v.mae / span),
        _ => None,
    };
    if let Some(rel) = relative_mae
        && rel > 0.25
    {
        reasons.push(format!(
            "held-out mean error is {:.0}% of the reference span",
            rel * 100.0
        ));
    }

    // Insufficient: two points with no validation and no independent reference
    // is a line through a guess.
    if input.n < 3 && !input.independent_validation {
        reasons.push("only two points and no independent validation".to_string());
        return (CalibrationQuality::InsufficientEvidence, reasons);
    }

    let strong = input.independent_reference
        && input.independent_validation
        && input.n >= 5
        && input.hardware_matches
        && coverage >= 0.4
        && relative_mae.map(|r| r <= 0.15).unwrap_or(false);
    if strong {
        reasons.push("independent reference with small held-out error".to_string());
        return (CalibrationQuality::Validated, reasons);
    }

    let weak = !input.hardware_matches
        || input.validation.is_none()
        || coverage < 0.25
        || relative_mae.map(|r| r > 0.25).unwrap_or(false)
        || input.n < 4;
    if weak {
        if reasons.is_empty() {
            reasons.push("limited evidence (few points or no independent validation)".to_string());
        }
        return (CalibrationQuality::Weak, reasons);
    }

    if reasons.is_empty() {
        reasons.push("usable: independent reference or adequate coverage".to_string());
    }
    (CalibrationQuality::UsableWithCaveats, reasons)
}

/// The domain a calibration was fitted for. Used to flag extrapolation instead
/// of silently treating it as equally reliable.
#[derive(Debug, Clone, PartialEq)]
pub struct Applicability {
    pub machine: String,
    pub panel: String,
    pub input_min: f64,
    pub input_max: f64,
    pub power_source: Option<String>,
}

impl Applicability {
    /// Reason the calibration does not apply to the requested input/machine, if
    /// any. Extrapolation is flagged, not hidden.
    pub fn check(&self, machine: &str, panel: &str, input: f64) -> Option<String> {
        if let Some(reason) = calib::identity_mismatch_reason(
            &DisplayModel {
                machine: self.machine.clone(),
                panel: self.panel.clone(),
                min_brightness: self.input_min,
                slope_w_per_pct: 0.0,
                intercept_w: 0.0,
                r2: 1.0,
                created_ms: 0,
                levels: Vec::new(),
            },
            machine,
            panel,
        ) {
            return Some(reason);
        }
        if input < self.input_min || input > self.input_max {
            return Some(format!(
                "extrapolation: {input:.1}% is outside the calibrated range {:.1}–{:.1}%",
                self.input_min, self.input_max
            ));
        }
        None
    }
}

/// Build a legacy [`DisplayModel`] from a fit so collectors and the CLI can use
/// the calibrated relationship unchanged. The model remains an estimate.
pub fn display_model_from_fit(
    fit: &FitOutcome,
    machine: &str,
    panel: &str,
    created_ms: u64,
    levels: Vec<(f64, f64)>,
) -> DisplayModel {
    DisplayModel {
        machine: machine.to_string(),
        panel: panel.to_string(),
        min_brightness: fit.input_min,
        slope_w_per_pct: fit.slope,
        intercept_w: fit.intercept,
        r2: fit.r2_fit,
        created_ms,
        levels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perfect() -> Vec<(f64, f64)> {
        // y = 0.05x + 5 at six levels.
        [0.0, 20.0, 40.0, 60.0, 80.0, 100.0]
            .iter()
            .map(|x| (*x, 0.05 * x + 5.0))
            .collect()
    }

    #[test]
    fn perfect_fit_recovers_line_and_is_validated() {
        let fit = fit_display_calibration(&perfect()).unwrap();
        assert!((fit.slope - 0.05).abs() < 1e-9);
        assert!((fit.intercept - 5.0).abs() < 1e-9);
        assert!(fit.independent_validation);
        let v = fit.validation.as_ref().unwrap();
        assert!(v.mae < 1e-9, "perfect held-out fit: {v:?}");
        let (q, _) = classify(&QualityInput {
            n: fit.n,
            independent_validation: fit.independent_validation,
            validation: fit.validation.clone(),
            reference_span: Some(5.0),
            input_min: fit.input_min,
            input_max: fit.input_max,
            target_domain: (0.0, 100.0),
            independent_reference: true,
            hardware_matches: true,
        });
        assert_eq!(q, CalibrationQuality::Validated);
    }

    #[test]
    fn noisy_fit_is_not_validated() {
        let mut points = perfect();
        // Perturb the two held-out points (x=40 and x=100) so the held-out
        // error is what drives the verdict, not the training residuals.
        points[2].1 += 3.0;
        points[5].1 -= 3.0;
        let fit = fit_display_calibration(&points).unwrap();
        assert!(fit.validation.is_some());
        let (q, reasons) = classify(&QualityInput {
            n: fit.n,
            independent_validation: true,
            validation: fit.validation.clone(),
            reference_span: Some(5.0),
            input_min: fit.input_min,
            input_max: fit.input_max,
            target_domain: (0.0, 100.0),
            independent_reference: true,
            hardware_matches: true,
        });
        assert_ne!(q, CalibrationQuality::Validated);
        assert!(!reasons.is_empty());
    }

    #[test]
    fn insufficient_data_is_explicit() {
        // A single point cannot be fitted at all.
        assert!(fit_display_calibration(&[(50.0, 8.0)]).is_none());
        // Two points fit a line but carry no independent validation.
        let fit = fit_display_calibration(&[(0.0, 5.0), (100.0, 10.0)]).unwrap();
        assert!(!fit.independent_validation);
        assert!(fit.validation.is_none());
        let (q, _) = classify(&QualityInput {
            n: fit.n,
            independent_validation: false,
            validation: None,
            reference_span: Some(5.0),
            input_min: 0.0,
            input_max: 100.0,
            target_domain: (0.0, 100.0),
            independent_reference: true,
            hardware_matches: true,
        });
        assert_eq!(q, CalibrationQuality::InsufficientEvidence);
    }

    #[test]
    fn held_out_validation_is_not_training_fit() {
        let mut points = perfect();
        points[3].1 += 3.0; // one bad held-out point
        let fit = fit_display_calibration(&points).unwrap();
        let v = fit.validation.expect("split present");
        // The training fit can still be near-perfect while held-out error shows
        // the miss; the two numbers must not be conflated.
        assert!(v.mae > 0.0, "held-out error must reflect the bad point");
        assert!(fit.r2_fit >= 0.0);
    }

    #[test]
    fn zero_reference_has_no_relative_error() {
        let fit = fit_display_calibration(&[(0.0, 0.0), (50.0, 2.5), (100.0, 5.0)]).unwrap();
        let at_zero = fit.residuals.iter().find(|r| r.input == 0.0).unwrap();
        assert_eq!(at_zero.relative_error, None);
        let at_hundred = fit.residuals.iter().find(|r| r.input == 100.0).unwrap();
        assert!(at_hundred.relative_error.is_some());
    }

    #[test]
    fn extrapolation_is_flagged() {
        let app = Applicability {
            machine: "M".to_string(),
            panel: "P".to_string(),
            input_min: 20.0,
            input_max: 80.0,
            power_source: Some("battery".to_string()),
        };
        assert!(app.check("M", "P", 50.0).is_none());
        let reason = app.check("M", "P", 95.0).unwrap();
        assert!(reason.contains("extrapolation"), "{reason}");
        // Identity mismatch outranks range.
        assert!(
            app.check("OTHER", "P", 50.0)
                .unwrap()
                .contains("machine mismatch")
        );
    }
}
