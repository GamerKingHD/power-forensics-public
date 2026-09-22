//! Persisted application settings.
//!
//! Corrupt or missing settings NEVER prevent the application from starting: the
//! loader falls back to defaults on any read/parse error. Only genuinely
//! supported options are exposed; a change that needs a restart says so, and a
//! path that cannot be created is rejected at save time rather than failing
//! later during an export.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::dto::BridgeError;

pub const SETTINGS_FILE: &str = "pf-gui-settings.json";
pub const MIN_CHART_INTERVAL_MS: u64 = 250;
pub const MAX_CHART_INTERVAL_MS: u64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    /// Preset passed to the agent when the UI starts monitoring.
    #[serde(default)]
    pub default_preset: Option<String>,
    /// Collector list passed to the agent when the UI starts monitoring.
    #[serde(default)]
    pub default_collectors: Option<String>,
    /// Whether new reports/exports default to redacted.
    #[serde(default)]
    pub redaction_default: bool,
    /// "system" | "light" | "dark"
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Directory for generated reports/exports. `None` = sessions/reports.
    #[serde(default)]
    pub report_export_dir: Option<String>,
    /// Live view polling interval, within a supported bound.
    #[serde(default = "default_chart_interval")]
    pub chart_interval_ms: u64,
}

fn default_theme() -> String {
    "system".to_string()
}

fn default_chart_interval() -> u64 {
    1000
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            default_preset: None,
            default_collectors: None,
            redaction_default: false,
            theme: default_theme(),
            report_export_dir: None,
            chart_interval_ms: default_chart_interval(),
        }
    }
}

pub fn settings_path(sessions_dir: &Path) -> PathBuf {
    sessions_dir.join(SETTINGS_FILE)
}

/// Structural validation. Returns a human reason for the first invalid field.
/// Deliberately does NOT touch the filesystem: a transiently unavailable export
/// path must not make an otherwise-valid settings file look corrupt.
pub fn validate(s: &AppSettings) -> Result<(), String> {
    if !matches!(s.theme.as_str(), "system" | "light" | "dark") {
        return Err(format!("unsupported theme '{}'", s.theme));
    }
    if !(MIN_CHART_INTERVAL_MS..=MAX_CHART_INTERVAL_MS).contains(&s.chart_interval_ms) {
        return Err(format!(
            "chart interval must be {MIN_CHART_INTERVAL_MS}–{MAX_CHART_INTERVAL_MS} ms"
        ));
    }
    Ok(())
}

/// Validate and confirm the export directory can actually be created. Only
/// called when saving, so a bad path is rejected before it can fail an export.
pub fn validate_writable(s: &AppSettings) -> Result<(), String> {
    validate(s)?;
    if let Some(dir) = &s.report_export_dir
        && !dir.is_empty()
    {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("report export directory is not writable: {e}"))?;
    }
    Ok(())
}

/// Load settings, falling back to defaults on any failure. This function is
/// intentionally infallible so a corrupt file cannot block startup.
pub fn load(sessions_dir: &Path) -> AppSettings {
    let Ok(text) = std::fs::read_to_string(settings_path(sessions_dir)) else {
        return AppSettings::default();
    };
    let Ok(parsed) = serde_json::from_str::<AppSettings>(&text) else {
        crate::logging::warn("settings file is corrupt; falling back to defaults");
        return AppSettings::default();
    };
    if let Err(reason) = validate(&parsed) {
        crate::logging::warn(format!(
            "settings file failed validation ({reason}); falling back to defaults"
        ));
        return AppSettings::default();
    }
    parsed
}

/// Persist settings atomically (temp + rename).
pub fn save(sessions_dir: &Path, settings: &AppSettings) -> Result<(), BridgeError> {
    validate_writable(settings).map_err(|e| BridgeError::new("bad-request", e))?;
    let path = settings_path(sessions_dir);
    let text = serde_json::to_string_pretty(settings).map_err(|e| {
        BridgeError::new("internal", "cannot serialize settings").with_detail(e.to_string())
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| {
        BridgeError::new("io-error", "cannot write settings").with_detail(e.to_string())
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        BridgeError::new("io-error", "cannot finalize settings").with_detail(e.to_string())
    })?;
    Ok(())
}

/// Directory reports/exports are written to, created if needed.
pub fn report_dir(sessions_dir: &Path, settings: &AppSettings) -> Result<PathBuf, BridgeError> {
    let dir = match &settings.report_export_dir {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => sessions_dir.join("reports"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| {
        BridgeError::new("io-error", "cannot create report directory").with_detail(e.to_string())
    })?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pf-gui-settings-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip() {
        let dir = temp_dir("round");
        let s = AppSettings {
            theme: "dark".to_string(),
            chart_interval_ms: 2000,
            redaction_default: true,
            ..Default::default()
        };
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir), s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_settings_fall_back_to_defaults() {
        let dir = temp_dir("corrupt");
        std::fs::write(settings_path(&dir), "{not json").unwrap();
        assert_eq!(load(&dir), AppSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_values_are_rejected() {
        let dir = temp_dir("invalid");
        let bad_theme = AppSettings {
            theme: "neon".to_string(),
            ..Default::default()
        };
        assert!(save(&dir, &bad_theme).is_err());
        let bad_interval = AppSettings {
            chart_interval_ms: 5,
            ..Default::default()
        };
        assert!(save(&dir, &bad_interval).is_err());
        // A persisted file with an invalid value also falls back on load.
        std::fs::write(
            settings_path(&dir),
            r#"{"theme":"neon","chartIntervalMs":10}"#,
        )
        .unwrap();
        assert_eq!(load(&dir), AppSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_export_dir_is_rejected() {
        let dir = temp_dir("unwritable");
        // A path whose parent is a file cannot be a directory.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let s = AppSettings {
            report_export_dir: Some(blocker.join("reports").to_string_lossy().to_string()),
            ..Default::default()
        };
        assert!(save(&dir, &s).is_err(), "must reject an uncreatable path");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
