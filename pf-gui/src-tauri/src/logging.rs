//! Restrained, bounded application log.
//!
//! Purpose: make field failures diagnosable from a durable file without
//! logging telemetry streams, session payloads, report contents or secrets.
//! Logging is best-effort: any failure (no directory permission, locked file)
//! is swallowed and can never prevent startup or a command from running.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE: &str = "pf-gui.log";
/// Rotation bound: at most ~1.5 MB per install (3 files x 512 KB).
const MAX_LOG_BYTES: u64 = 512 * 1024;
const RETAINED: usize = 3;

fn state() -> &'static Mutex<Option<PathBuf>> {
    static DIR: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    DIR.get_or_init(|| Mutex::new(None))
}

/// Stable per-user log directory (or beside a portable executable). Never
/// inside the user's evidence directory, so clearing logs can never touch
/// recorded sessions.
pub fn log_dir() -> PathBuf {
    crate::sessions::user_data_root().join("logs")
}

/// Enable logging. Returns the resolved directory. Failure is non-fatal.
pub fn init() -> PathBuf {
    let dir = log_dir();
    if fs::create_dir_all(&dir).is_ok()
        && let Ok(mut guard) = state().lock()
    {
        *guard = Some(dir.clone());
    }
    dir
}

pub fn info(message: impl AsRef<str>) {
    write("INFO", message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    write("WARN", message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    write("ERROR", message.as_ref());
}

fn write(level: &str, message: &str) {
    let guard = match state().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let Some(dir) = guard.as_ref() else {
        return;
    };
    rotate_if_needed(dir);
    let line = format!("{} [{level}] {}\n", timestamp(), message);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Open the log directory in the OS file browser. Best-effort.
pub fn open_log_dir() -> Result<(), String> {
    let dir = log_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        std::process::Command::new(opener)
            .arg(&dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn rotate_if_needed(dir: &Path) {
    let path = dir.join(LOG_FILE);
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if size < MAX_LOG_BYTES {
        return;
    }
    let _ = fs::remove_file(dir.join(format!("{LOG_FILE}.{}", RETAINED - 1)));
    for i in (1..RETAINED - 1).rev() {
        let _ = fs::rename(
            dir.join(format!("{LOG_FILE}.{i}")),
            dir.join(format!("{LOG_FILE}.{}", i + 1)),
        );
    }
    let _ = fs::rename(&path, dir.join(format!("{LOG_FILE}.1")));
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Days since the Unix epoch to a civil UTC date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_a_valid_utc_instant() {
        // 2021-01-01T00:00:00Z == 1609459200 == 18628 days
        assert_eq!(civil_from_days(18_628), (2021, 1, 1));
    }

    #[test]
    fn logging_is_a_noop_until_initialized_and_never_panics() {
        // Not initialized in this test process: must not panic or create files.
        info("test line");
        warn("test line");
        error("test line");
    }

    #[test]
    fn rotation_keeps_a_bounded_number_of_files() {
        let dir = std::env::temp_dir().join(format!("pf-gui-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOG_FILE);
        fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        rotate_if_needed(&dir);
        rotate_if_needed(&dir);
        let count = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(LOG_FILE))
            .count();
        assert!(count <= RETAINED, "log files are bounded, found {count}");
        let _ = fs::remove_dir_all(&dir);
    }
}
