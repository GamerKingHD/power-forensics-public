//! Display-power calibration.
//!
//! Display watts are never invented. A controlled brightness sweep measures
//! the idle discharge at each level; a least-squares line is fitted and, when
//! the fit is good enough, persisted with the panel/machine identity. Only a
//! matching, valid model may turn brightness into an *incremental* display
//! power estimate, and that estimate is always labelled derived.
//!
//! Units: brightness in percent, power in watts. The intercept is the
//! non-display base load at 0% brightness, so the model's display component
//! is `slope * (brightness - min_brightness)`.

use crate::json::JVal;
use crate::telemetry::escape_json;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearFit {
    pub slope: f64,
    pub intercept: f64,
    /// Coefficient of determination in 0..=1; meaningless with < 2 points.
    pub r2: f64,
    pub n: usize,
}

/// Least-squares fit of y = slope*x + intercept. None for < 2 points or a
/// degenerate x spread.
pub fn fit_linear(points: &[(f64, f64)]) -> Option<LinearFit> {
    let n = points.len();
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let sx: f64 = points.iter().map(|(x, _)| *x).sum();
    let sy: f64 = points.iter().map(|(_, y)| *y).sum();
    let sxx: f64 = points.iter().map(|(x, _)| x * x).sum();
    let sxy: f64 = points.iter().map(|(x, y)| x * y).sum();
    let denom = nf * sxx - sx * sx;
    if denom.abs() < f64::EPSILON {
        return None;
    }
    let slope = (nf * sxy - sx * sy) / denom;
    let intercept = (sy - slope * sx) / nf;
    let mean_y = sy / nf;
    let ss_tot: f64 = points.iter().map(|(_, y)| (y - mean_y).powi(2)).sum();
    let ss_res: f64 = points
        .iter()
        .map(|(x, y)| (y - (intercept + slope * x)).powi(2))
        .sum();
    let r2 = if ss_tot.abs() < f64::EPSILON {
        // Constant y: a flat line fits perfectly, but proves nothing.
        0.0
    } else {
        (1.0 - ss_res / ss_tot).clamp(0.0, 1.0)
    };
    Some(LinearFit {
        slope,
        intercept,
        r2,
        n,
    })
}

/// A persisted, machine-specific calibration.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayModel {
    pub machine: String,
    pub panel: String,
    /// Percent brightness where the sweep started (display component is zero).
    pub min_brightness: f64,
    pub slope_w_per_pct: f64,
    pub intercept_w: f64,
    pub r2: f64,
    pub created_ms: u64,
    pub levels: Vec<(f64, f64)>,
}

/// A calibration is only trusted at or above this R².
pub const MIN_R2: f64 = 0.5;

impl DisplayModel {
    /// Incremental display power above the sweep's minimum brightness.
    /// Returns None when the model is not trustworthy.
    pub fn display_w_at(&self, brightness_pct: f64) -> Option<f64> {
        if self.display_w_unavailable_reason(brightness_pct).is_some() {
            return None;
        }
        Some(self.slope_w_per_pct * (brightness_pct - self.min_brightness).max(0.0))
    }

    /// Why `display_w_at` refuses to produce a value, if it does. A model is
    /// only usable when the fit is trusted (R²) AND physically valid: a
    /// negative slope would mean power falls as brightness rises, and a
    /// negative prediction would be nonsense. Unavailable, not zero.
    pub fn display_w_unavailable_reason(&self, brightness_pct: f64) -> Option<&'static str> {
        if self.r2 < MIN_R2 {
            return Some("R² below trust gate");
        }
        if !self.slope_w_per_pct.is_finite() || self.slope_w_per_pct < 0.0 {
            return Some("negative slope: power would decrease with brightness");
        }
        let predicted = self.slope_w_per_pct * (brightness_pct - self.min_brightness).max(0.0);
        if !predicted.is_finite() || predicted < 0.0 {
            return Some("predicted display power is negative or non-finite");
        }
        None
    }

    pub fn to_json(&self) -> String {
        let levels = self
            .levels
            .iter()
            .map(|(b, w)| format!("{{\"brightness\":{b},\"median_w\":{w}}}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"machine\":{},\"panel\":{},\"min_brightness\":{},\
             \"slope_w_per_pct\":{},\"intercept_w\":{},\"r2\":{},\"created_ms\":{},\"levels\":[{levels}]}}",
            json_string(&self.machine),
            json_string(&self.panel),
            self.min_brightness,
            self.slope_w_per_pct,
            self.intercept_w,
            self.r2,
            self.created_ms,
        )
    }

    pub fn from_json(v: &JVal) -> Option<DisplayModel> {
        let num = |k: &str| v.get(k).and_then(|x| x.num());
        let levels = v
            .get("levels")
            .and_then(|l| l.arr())
            .map(|a| {
                a.iter()
                    .filter_map(|p| Some((p.get("brightness")?.num()?, p.get("median_w")?.num()?)))
                    .collect()
            })
            .unwrap_or_default();
        Some(DisplayModel {
            machine: v.get("machine")?.as_str()?.to_string(),
            panel: v
                .get("panel")
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string(),
            min_brightness: num("min_brightness")?,
            slope_w_per_pct: num("slope_w_per_pct")?,
            intercept_w: num("intercept_w")?,
            r2: num("r2")?,
            created_ms: num("created_ms").unwrap_or(0.0) as u64,
            levels,
        })
    }
}

fn json_string(s: &str) -> String {
    format!("\"{}\"", escape_json(s))
}

/// Identity mismatch reason, if the stored calibration does not belong to
/// the current machine/panel. Empty stored identity is grandfathered
/// (legacy files predate identity): no reason, check skipped.
pub fn identity_mismatch_reason(
    model: &DisplayModel,
    machine: &str,
    panel: &str,
) -> Option<String> {
    if !model.machine.is_empty() && model.machine != machine {
        return Some(format!(
            "machine mismatch: stored '{}' vs current '{}'",
            model.machine, machine
        ));
    }
    if !model.panel.is_empty() && model.panel != panel {
        return Some(format!(
            "panel mismatch: stored '{}' vs current '{}'",
            model.panel, panel
        ));
    }
    None
}

/// Load + identity-check a calibration file. Ok(model) only when the stored
/// machine/panel matches the passed-in current values (or the stored
/// identity is empty, the legacy compat case). Err(reason) on missing file,
/// bad JSON, or identity mismatch — a mismatched model must never turn
/// another machine's brightness into watts.
pub fn load_display_model_from(
    path: &str,
    machine: &str,
    panel: &str,
) -> Result<DisplayModel, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let v = crate::json::parse(&text).map_err(|e| format!("bad calibration JSON: {e}"))?;
    let model = DisplayModel::from_json(&v).ok_or_else(|| "bad calibration JSON".to_string())?;
    if let Some(reason) = identity_mismatch_reason(&model, machine, panel) {
        return Err(reason);
    }
    Ok(model)
}

/// Default-path loader with identity check. Returns None on any failure
/// (missing file, bad JSON, identity mismatch). NOTE to main.rs: the
/// reps==1/compare/report paths should call this with the current
/// COMPUTERNAME/panel instead of the unchecked loader, so a calibration
/// from another machine can never apply.
pub fn load_display_model_for(machine: &str, panel: &str) -> Option<DisplayModel> {
    load_display_model_from("sessions/display-calibration.json", machine, panel).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_fit_recovers_known_line() {
        // y = 2x + 3
        let pts: Vec<(f64, f64)> = (0..=5).map(|i| (i as f64, 2.0 * i as f64 + 3.0)).collect();
        let f = fit_linear(&pts).unwrap();
        assert!((f.slope - 2.0).abs() < 1e-9);
        assert!((f.intercept - 3.0).abs() < 1e-9);
        assert!((f.r2 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn linear_fit_rejects_degenerate() {
        assert!(fit_linear(&[]).is_none());
        assert!(fit_linear(&[(0.0, 1.0)]).is_none());
        // All x equal -> no slope.
        assert!(fit_linear(&[(5.0, 1.0), (5.0, 2.0)]).is_none());
    }

    #[test]
    fn display_model_gates_on_r2() {
        let m = DisplayModel {
            machine: "m".to_string(),
            panel: "p".to_string(),
            min_brightness: 0.0,
            slope_w_per_pct: 0.05,
            intercept_w: 5.0,
            r2: 0.9,
            created_ms: 0,
            levels: vec![(0.0, 5.0), (100.0, 10.0)],
        };
        assert!((m.display_w_at(100.0).unwrap() - 5.0).abs() < 1e-9);
        let bad = DisplayModel {
            r2: 0.1,
            ..m.clone()
        };
        assert!(bad.display_w_at(50.0).is_none());
        assert!(
            bad.display_w_unavailable_reason(50.0)
                .unwrap()
                .contains("R²")
        );
    }

    #[test]
    fn display_model_rejects_negative_slope() {
        let m = DisplayModel {
            machine: "m".to_string(),
            panel: "p".to_string(),
            min_brightness: 0.0,
            slope_w_per_pct: -0.05,
            intercept_w: 5.0,
            r2: 0.9,
            created_ms: 0,
            levels: vec![(0.0, 3.0), (100.0, 8.0)],
        };
        // A negative fit must never turn brightness into negative watts.
        assert!(m.display_w_at(100.0).is_none());
        assert!(
            m.display_w_unavailable_reason(100.0)
                .unwrap()
                .contains("negative slope")
        );
    }

    #[test]
    fn display_model_valid_and_zero() {
        let m = DisplayModel {
            machine: "m".to_string(),
            panel: "p".to_string(),
            min_brightness: 20.0,
            slope_w_per_pct: 0.05,
            intercept_w: 5.0,
            r2: 0.9,
            created_ms: 0,
            levels: vec![(20.0, 5.0), (100.0, 9.0)],
        };
        assert!(m.display_w_unavailable_reason(60.0).is_none());
        assert!((m.display_w_at(60.0).unwrap() - 2.0).abs() < 1e-9);
        // At or below the sweep minimum the display component is zero.
        assert_eq!(m.display_w_at(20.0), Some(0.0));
        assert_eq!(m.display_w_at(5.0), Some(0.0));
        // Zero slope is flat but non-negative: valid, predicts zero.
        let flat = DisplayModel {
            slope_w_per_pct: 0.0,
            r2: 0.9,
            ..m.clone()
        };
        assert_eq!(flat.display_w_at(100.0), Some(0.0));
    }

    #[test]
    fn display_model_json_roundtrip() {
        let m = DisplayModel {
            machine: "ASUS \"X\"".to_string(),
            panel: "AUO".to_string(),
            min_brightness: 20.0,
            slope_w_per_pct: 0.041,
            intercept_w: 4.8,
            r2: 0.83,
            created_ms: 123,
            levels: vec![(20.0, 5.3), (60.0, 7.2), (100.0, 8.9)],
        };
        let parsed = crate::json::parse(&m.to_json()).unwrap();
        assert_eq!(DisplayModel::from_json(&parsed).unwrap(), m);
    }

    fn write_calib(path: &std::path::Path, machine: &str, panel: &str) {
        let m = DisplayModel {
            machine: machine.to_string(),
            panel: panel.to_string(),
            min_brightness: 0.0,
            slope_w_per_pct: 0.05,
            intercept_w: 5.0,
            r2: 0.9,
            created_ms: 0,
            levels: vec![(0.0, 5.0), (100.0, 10.0)],
        };
        std::fs::write(path, m.to_json()).unwrap();
    }

    #[test]
    fn identity_gates_calibration_reuse() {
        let dir = std::env::temp_dir();
        let path = dir.join("testttt-calib-identity.json");
        let p = path.to_string_lossy().to_string();
        // Matching identity loads.
        write_calib(&path, "MINE", "AUO");
        assert!(load_display_model_from(&p, "MINE", "AUO").is_ok());
        // Machine mismatch refuses with a reason.
        let err = load_display_model_from(&p, "OTHER", "AUO").unwrap_err();
        assert!(err.contains("machine mismatch"), "{err}");
        // Panel mismatch refuses with a reason.
        let err = load_display_model_from(&p, "MINE", "BOE").unwrap_err();
        assert!(err.contains("panel mismatch"), "{err}");
        // Empty stored identity is grandfathered (legacy compat).
        write_calib(&path, "", "");
        assert!(load_display_model_from(&p, "ANY", "ANY").is_ok());
        // Missing file is an error, not a model.
        assert!(
            load_display_model_from(
                &dir.join("testttt-no-such-calib.json").to_string_lossy(),
                "M",
                "P"
            )
            .is_err()
        );
        let _ = std::fs::remove_file(&path);
    }
}
