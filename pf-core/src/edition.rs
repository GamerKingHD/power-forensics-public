//! Edition and capability detection.
//!
//! power-forensics is built as a self-sufficient community edition. Optional
//! premium functionality, if present, ships as a separate component installed
//! alongside the application and is detected at runtime. No community crate
//! links against premium code, so removing that component can never break the
//! community build.
//!
//! This module is the single seam that knows a premium component may exist. It
//! only looks for an inert marker file; it never imports, loads, or links any
//! premium implementation.

use std::path::{Path, PathBuf};

/// Marker dropped next to the executable by the premium component.
pub const PRO_MARKER: &str = "power-forensics.pro";

/// Which edition of the application is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Community,
    Pro,
}

impl Edition {
    pub fn is_pro(self) -> bool {
        matches!(self, Edition::Pro)
    }
}

/// Result of a capability probe, including a human-readable explanation.
#[derive(Debug, Clone)]
pub struct EditionStatus {
    pub edition: Edition,
    pub detail: String,
}

impl EditionStatus {
    pub fn is_pro(&self) -> bool {
        self.edition.is_pro()
    }
}

/// Detect the active edition relative to the current directory.
pub fn detect() -> EditionStatus {
    detect_in(Path::new("."))
}

/// Detect the active edition for a specific installation directory.
///
/// Detection fails closed to [`Edition::Community`]: a missing, moved, or
/// unreadable component is never an error and never disables core features.
pub fn detect_in(dir: &Path) -> EditionStatus {
    let marker: PathBuf = dir.join(PRO_MARKER);
    if marker.is_file() {
        EditionStatus {
            edition: Edition::Pro,
            detail: format!("premium component detected ({})", marker.display()),
        }
    } else {
        EditionStatus {
            edition: Edition::Community,
            detail: "community edition".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_marker_is_community() {
        let status = detect_in(Path::new("definitely-not-a-real-directory-xyz"));
        assert_eq!(status.edition, Edition::Community);
        assert!(!status.is_pro());
    }

    #[test]
    fn present_marker_is_pro() {
        let dir = std::env::temp_dir().join(format!("pf-edition-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PRO_MARKER), b"").unwrap();
        let status = detect_in(&dir);
        assert_eq!(status.edition, Edition::Pro);
        assert!(status.is_pro());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
