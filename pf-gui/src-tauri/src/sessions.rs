//! Session discovery, listing, summary and bounded window access.
//!
//! Listing is served from the derived [`crate::index`] cache (rebuilt from the
//! authoritative JSONL whenever it is missing or stale) plus a streaming
//! substring scan, so a directory of long recordings stays cheap to browse.
//!
//! Window access is bounded: a one-time scan records timestamp/byte-offset
//! checkpoints, then a requested interval seeks near its start and parses only
//! the region it needs. The full file is never reparsed for a pan/zoom.
//!
//! Summary views are built from the file tail plus the index, not a full
//! materialization. All forensic values still come from the authoritative
//! `pf_core::session` loader; nothing is re-derived here.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use pf_core::json::parse;
use pf_core::session::{self, SessionData};

use crate::dto::{BridgeError, CapabilityReport, SessionListEntry, SessionSummary, SessionWindow};
use crate::evidence;
use crate::index::{self, Fingerprint, SessionIndex};

const HEAD_BYTES: u64 = 4 * 1024;
const TAIL_BYTES: u64 = 128 * 1024;
pub const MAX_WINDOW_POINTS: usize = 4_000;
/// One checkpoint per this many records keeps the in-memory index small while
/// still bounding the region parsed for a window.
const CHECKPOINT_EVERY_LINES: u64 = 256;

/// Marker file shipped next to a portable executable. Its presence opts the
/// build into storing sessions (and logs) beside the executable instead of the
/// per-user data directory.
pub const PORTABLE_MARKER: &str = "power-forensics.portable";

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// Root directory for per-user writable state (sessions, settings, logs). The
/// installed application must never write inside its install directory, which
/// may be under `Program Files` for a per-machine install. Portable builds
/// (marker next to the executable) deliberately keep state beside the binary.
pub fn user_data_root() -> PathBuf {
    if let Ok(d) = std::env::var("PF_DATA_DIR")
        && !d.trim().is_empty()
    {
        return PathBuf::from(d);
    }
    if let Some(dir) = exe_dir()
        && dir.join(PORTABLE_MARKER).is_file()
    {
        return dir;
    }
    if let Ok(base) = std::env::var("LOCALAPPDATA")
        && !base.trim().is_empty()
    {
        return PathBuf::from(base).join("power-forensics");
    }
    std::env::temp_dir().join("power-forensics")
}

/// Resolve the session directory: explicit override, portable marker, a
/// development workspace `sessions/` when the executable lives under a Cargo
/// `target/` tree, otherwise the per-user data directory. An installed binary
/// never adopts a CWD-relative `sessions` directory. Read-only; never created.
pub fn resolve_sessions_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PF_SESSIONS_DIR")
        && !d.trim().is_empty()
    {
        return PathBuf::from(d);
    }
    let exe = exe_dir();
    if let Some(dir) = &exe
        && dir.join(PORTABLE_MARKER).is_file()
    {
        return dir.join("sessions");
    }
    if let Some(dir) = &exe
        && is_cargo_target_dir(dir)
    {
        let cwd = std::env::current_dir().ok();
        if let Some(existing) = existing_sessions_dir(cwd.as_deref(), Some(dir)) {
            return existing;
        }
    }
    user_data_root().join("sessions")
}

/// True when `dir` is a Cargo profile directory (`target/debug`,
/// `target/release`), i.e. an uninstalled development build.
fn is_cargo_target_dir(dir: &Path) -> bool {
    let profile = matches!(
        dir.file_name().and_then(|n| n.to_str()),
        Some("debug" | "release")
    );
    let under_target = dir
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|n| n == "target")
        .unwrap_or(false);
    profile && under_target
}

/// An already-present sessions directory (workspace development layout). A
/// clean install has none, so it falls through to the per-user data directory.
fn existing_sessions_dir(cwd: Option<&Path>, exe_dir: Option<&Path>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(cwd) = cwd {
        candidates.push(cwd.join("sessions"));
        candidates.push(cwd.join("..").join("..").join("sessions"));
    }
    if let Some(dir) = exe_dir {
        candidates.push(dir.join("sessions"));
        candidates.push(dir.join("..").join("..").join("sessions"));
    }
    candidates.into_iter().find(|p| p.is_dir())
}

/// Read the head and tail of a session file without loading the middle. A
/// trailing partial line (a write in progress) is dropped so the strict
/// loader is never handed a torn record.
pub(crate) fn read_head_tail(path: &Path) -> Result<(String, String, u64), BridgeError> {
    let mut file = std::fs::File::open(path).map_err(|e| {
        BridgeError::new("io-error", format!("cannot open {}", path.display()))
            .with_detail(e.to_string())
    })?;
    let len = file
        .metadata()
        .map(|m| m.len())
        .map_err(|e| BridgeError::new("io-error", e.to_string()))?;

    let mut head = vec![0u8; HEAD_BYTES.min(len) as usize];
    file.read_exact(&mut head)
        .map_err(|e| BridgeError::new("io-error", e.to_string()))?;
    let head = String::from_utf8_lossy(&head).to_string();

    let mut tail = if len <= TAIL_BYTES {
        let mut all = String::new();
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.read_to_string(&mut all))
            .map_err(|e| BridgeError::new("io-error", e.to_string()))?;
        all
    } else {
        file.seek(SeekFrom::Start(len - TAIL_BYTES))
            .map_err(|e| BridgeError::new("io-error", e.to_string()))?;
        let mut buf = Vec::with_capacity(TAIL_BYTES as usize);
        file.read_to_end(&mut buf)
            .map_err(|e| BridgeError::new("io-error", e.to_string()))?;
        let text = String::from_utf8_lossy(&buf).to_string();
        // Drop the first (possibly partial) line of the tail.
        match text.find('\n') {
            Some(i) => text[i + 1..].to_string(),
            None => String::new(),
        }
    };
    if !tail.is_empty() && !tail.ends_with('\n') {
        match tail.rfind('\n') {
            Some(i) => tail.truncate(i + 1),
            None => tail.clear(),
        }
    }
    Ok((head, tail, len))
}

fn header_line(head: &str) -> Option<String> {
    head.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

pub(crate) fn header_json(head: &str) -> Option<pf_core::json::JVal> {
    header_line(head).and_then(|l| parse(&l).ok())
}

/// Load only the tail of a live session (bounded), returning the parsed
/// samples plus the header text and the file size. The authoritative loader is
/// used; no sample parsing is reimplemented here.
pub fn read_tail_session(path: &Path) -> Result<(SessionData, String, u64), BridgeError> {
    let (head, tail, size) = read_head_tail(path)?;
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let s = session::load_session(&label, &tail).map_err(|e| {
        BridgeError::new("session-corrupt", "cannot parse live session tail").with_detail(e)
    })?;
    Ok((s, head, size))
}

pub(crate) fn footer_line(tail: &str) -> Option<String> {
    tail.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rfind(|l| {
            parse(l)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(|t| t.as_str())
                        .map(|t| t == "session_footer")
                })
                .unwrap_or(false)
        })
        .map(str::to_string)
}

/// Canonicalize a path the agent published as its active session and confirm
/// it lives inside the configured session directory. Rejecting anything else
/// (including a non-`.jsonl` file) means the GUI never silently reads an
/// unrelated file when an externally launched agent writes elsewhere.
pub fn confine_active_session(
    sessions_dir: &Path,
    requested: &str,
) -> Result<PathBuf, BridgeError> {
    let candidate = PathBuf::from(requested);
    let base = sessions_dir.canonicalize().map_err(|e| {
        BridgeError::new("io-error", "session directory is unavailable").with_detail(e.to_string())
    })?;
    let target = candidate.canonicalize().map_err(|e| {
        BridgeError::new("not-found", "active session file is not readable")
            .with_detail(e.to_string())
    })?;
    if !target.starts_with(&base) {
        return Err(BridgeError::new(
            "session-outside-dir",
            "active session is outside the configured GUI session directory",
        ));
    }
    if target.extension().map(|x| x != "jsonl").unwrap_or(true) {
        return Err(BridgeError::new(
            "bad-request",
            "active session path is not a .jsonl recording",
        ));
    }
    Ok(target)
}

pub fn list_sessions(dir: &Path) -> Result<Vec<SessionListEntry>, BridgeError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        BridgeError::new("io-error", format!("cannot list {}", dir.display()))
            .with_detail(e.to_string())
    })?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort();

    let mut cache = SessionIndex::load(dir).unwrap_or_else(|| SessionIndex::empty(dir));
    let mut changed = cache.entries.len() != files.len();
    let mut out: Vec<SessionListEntry> = Vec::with_capacity(files.len());
    for path in files {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let fingerprint = Fingerprint::of(&path);
        let reusable = fingerprint.and_then(|fp| {
            cache
                .entries
                .iter()
                .find(|e| e.file == name && e.bytes == fp.bytes && e.mtime_s == Some(fp.mtime_s))
                .cloned()
        });
        match reusable {
            Some(entry) => out.push(entry),
            None => {
                changed = true;
                out.push(build_entry_or_unreadable(&path, &name));
            }
        }
    }
    if changed {
        cache.entries = out.clone();
        cache.save(dir);
    }
    Ok(out)
}

/// Force a full rebuild of the derived index from the authoritative JSONL and
/// return the refreshed listing. Used by the UI when a stale/corrupt index is
/// suspected; correctness never depended on the old cache.
pub fn rebuild_index(dir: &Path) -> Result<Vec<SessionListEntry>, BridgeError> {
    let path = index::index_path(dir);
    let _ = std::fs::remove_file(path);
    list_sessions(dir)
}

fn build_entry_or_unreadable(path: &Path, name: &str) -> SessionListEntry {
    match build_entry(path) {
        Ok(e) => e,
        // A corrupt/unreadable file is surfaced as an entry with an error
        // status rather than silently disappearing from the browser.
        Err(_) => SessionListEntry {
            file: name.to_string(),
            path: path.to_string_lossy().to_string(),
            label: name.to_string(),
            status: "unreadable".to_string(),
            mode: "unreadable".to_string(),
            ..Default::default()
        },
    }
}

fn build_entry(path: &Path) -> Result<SessionListEntry, BridgeError> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let (head, tail, size) = read_head_tail(path)?;
    let header = header_json(&head);
    let footer = footer_line(&tail).and_then(|l| parse(&l).ok());
    let meta = std::fs::metadata(path).ok();
    let mtime_s = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    let start_wall_ms = header
        .as_ref()
        .and_then(|v| v.get("wall_ms"))
        .and_then(|m| m.num())
        .unwrap_or(0.0) as u64;
    let note = header
        .as_ref()
        .and_then(|v| v.get("note"))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let interval_ms = header
        .as_ref()
        .and_then(|v| v.get("interval_ms"))
        .and_then(|m| m.num())
        .unwrap_or(0.0)
        .max(0.0) as u64;
    let has_header = header.is_some();

    let footer_wall = footer
        .as_ref()
        .and_then(|v| v.get("wall_ms"))
        .and_then(|m| m.num())
        .unwrap_or(0.0) as u64;
    let recovered = footer
        .as_ref()
        .map(|v| {
            v.get("recovered")
                .and_then(|b| b.as_bool())
                .unwrap_or(false)
                || v.get("summary")
                    .and_then(|s| s.get("recovered"))
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    let summary = footer.as_ref().and_then(|v| v.get("summary"));
    let snum = |k: &str| summary.and_then(|s| s.get(k)).and_then(|x| x.num());
    let samples = snum("samples").unwrap_or(0.0).max(0.0) as u64;

    let status = if footer.is_some() {
        "ok"
    } else if has_header {
        "incomplete"
    } else {
        "no-header"
    };
    let mode = if status != "ok" {
        "incomplete"
    } else if recovered {
        "recovered"
    } else {
        "recorded"
    };
    let end_wall_ms = if footer_wall > 0 {
        footer_wall
    } else {
        mtime_s.map(|s| s * 1000).unwrap_or(start_wall_ms)
    };
    let duration_s = if end_wall_ms > start_wall_ms {
        (end_wall_ms - start_wall_ms) as f64 / 1000.0
    } else {
        0.0
    };

    let scan = index::scan_details(path, size);
    let (markers, collectors) = match &scan {
        Some(scan) => (Some(scan.markers), scan.collectors.clone()),
        None => (None, Vec::new()),
    };
    // The footer carries the authoritative discontinuity/energy counts; the
    // streamed scan cannot reconstruct dual-clock boundaries.
    let mut stats = scan.map(|s| s.stats);
    if let Some(stats) = stats.as_mut() {
        stats.discontinuities = snum("discharge_discontinuities");
    }
    Ok(SessionListEntry {
        file: name.clone(),
        path: path.to_string_lossy().to_string(),
        label: name,
        note,
        start_wall_ms,
        end_wall_ms,
        duration_s,
        status: status.to_string(),
        recovered,
        mode: mode.to_string(),
        samples,
        interval_ms,
        discharge_median_w: snum("discharge_median_w"),
        discharge_wh: snum("discharge_wh"),
        charge_wh: snum("charge_wh"),
        cpu_median_pct: snum("cpu_utility_median_pct"),
        coverage_pct: snum("discharge_coverage_pct"),
        discontinuities: snum("discharge_discontinuities"),
        markers,
        collectors,
        stats,
        bytes: size,
        mtime_s,
    })
}

pub fn open_session_summary(
    path: &Path,
    caps: &CapabilityReport,
) -> Result<SessionSummary, BridgeError> {
    let (head, tail, size) = read_head_tail(path)?;
    let header = header_json(&head);
    let footer = footer_line(&tail).and_then(|l| parse(&l).ok());
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // The tail is the authoritative recent evidence; the middle is not needed
    // for a summary and is never materialized.
    let s = session::load_session(&label, &tail).map_err(|e| {
        BridgeError::new("session-corrupt", format!("cannot parse {label}")).with_detail(e)
    })?;
    let scan = index::scan_details(path, size);
    let scan_markers = scan.as_ref().map(|s| s.markers);
    let scan_collectors = scan.as_ref().map(|s| s.collectors.clone());
    let mtime_s = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());

    let interval_ms = header
        .as_ref()
        .and_then(|v| v.get("interval_ms"))
        .and_then(|m| m.num())
        .unwrap_or(0.0)
        .max(0.0) as u64;
    let start_wall_ms = header
        .as_ref()
        .and_then(|v| v.get("wall_ms"))
        .and_then(|m| m.num())
        .unwrap_or(s.wall_base_ms as f64) as u64;
    let footer_wall = footer
        .as_ref()
        .and_then(|v| v.get("wall_ms"))
        .and_then(|m| m.num())
        .unwrap_or(0.0) as u64;
    let end_wall_ms = if footer_wall > 0 {
        footer_wall
    } else {
        mtime_s.map(|s| s * 1000).unwrap_or(start_wall_ms)
    };
    let fsum = evidence::footer_summary(footer.as_ref().and_then(|v| v.get("summary")));
    let recovered = fsum.recovered;
    let status = if footer.is_some() { "ok" } else { "incomplete" };
    let samples = fsum
        .samples
        .map(|x| x.max(0.0) as u64)
        .unwrap_or(s.battery.len() as u64);

    let mut summary = SessionSummary {
        path: path.to_string_lossy().to_string(),
        label: s.label.clone(),
        note: header
            .as_ref()
            .and_then(|v| v.get("note"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string(),
        status: status.to_string(),
        recovered,
        start_wall_ms,
        end_wall_ms,
        duration_s: if end_wall_ms > start_wall_ms {
            (end_wall_ms - start_wall_ms) as f64 / 1000.0
        } else {
            0.0
        },
        interval_ms,
        samples,
        markers: scan_markers.unwrap_or(0),
        has_footer: footer.is_some(),
        battery: evidence::battery_view(&s),
        cpu: evidence::cpu_view(&s),
        gpus: evidence::gpu_views(&s),
        display: evidence::display_view(&s),
        system: evidence::system_view(&s),
        processes: evidence::process_rows(&s),
        collectors: evidence::collectors_view(
            &s,
            scan_collectors.as_deref(),
            interval_ms,
            caps,
            end_wall_ms,
        ),
        events: evidence::events_view(&s, 500),
        footer: fsum,
    };
    evidence::stamp_summary_ages(&mut summary, end_wall_ms);
    Ok(summary)
}

#[derive(Clone, Copy)]
struct Checkpoint {
    wall_ms: u64,
    byte_offset: u64,
}

struct WindowIndex {
    size: u64,
    checkpoints: Vec<Checkpoint>,
}

/// Bound on cached per-file window checkpoints. Checkpoints are small, but the
/// cache must not grow without limit as a long-lived GUI browses many sessions;
/// the JSONL remains authoritative, so eviction only costs a re-scan.
const MAX_WINDOW_INDEX_ENTRIES: usize = 8;

fn window_index_cache() -> &'static Mutex<HashMap<PathBuf, WindowIndex>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, WindowIndex>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Extract a top-level `"wall_ms":<n>` value with a bounded substring scan.
fn extract_wall_ms(line: &str) -> Option<u64> {
    let key = "\"wall_ms\":";
    let start = line.find(key)? + key.len();
    let rest = line[start..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse::<u64>().ok()
}

fn build_window_index(path: &Path, size: u64) -> Result<WindowIndex, BridgeError> {
    let file = std::fs::File::open(path).map_err(|e| {
        BridgeError::new("io-error", format!("cannot open {}", path.display()))
            .with_detail(e.to_string())
    })?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut line = Vec::new();
    let mut offset: u64 = 0;
    let mut line_no: u64 = 0;
    let mut checkpoints: Vec<Checkpoint> = Vec::new();
    loop {
        line.clear();
        let start = offset;
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| BridgeError::new("io-error", e.to_string()))?;
        if n == 0 {
            break;
        }
        offset += n as u64;
        if line_no.is_multiple_of(CHECKPOINT_EVERY_LINES)
            && let Some(wall_ms) = extract_wall_ms(&String::from_utf8_lossy(&line))
        {
            checkpoints.push(Checkpoint {
                wall_ms,
                byte_offset: start,
            });
        }
        line_no += 1;
    }
    Ok(WindowIndex { size, checkpoints })
}

fn get_window_index(path: &Path, size: u64) -> Result<WindowIndex, BridgeError> {
    let key = path.to_path_buf();
    if let Ok(cache) = window_index_cache().lock()
        && let Some(idx) = cache.get(&key)
        && idx.size == size
    {
        return Ok(WindowIndex {
            size: idx.size,
            checkpoints: idx.checkpoints.clone(),
        });
    }
    let idx = build_window_index(path, size)?;
    if let Ok(mut cache) = window_index_cache().lock() {
        if cache.len() >= MAX_WINDOW_INDEX_ENTRIES
            && !cache.contains_key(&key)
            && let Some(evict) = cache.keys().next().cloned()
        {
            cache.remove(&evict);
        }
        cache.insert(
            key,
            WindowIndex {
                size: idx.size,
                checkpoints: idx.checkpoints.clone(),
            },
        );
    }
    Ok(idx)
}

fn start_offset_for(checkpoints: &[Checkpoint], from_ms: u64) -> u64 {
    if from_ms == 0 || checkpoints.is_empty() {
        return 0;
    }
    // Largest checkpoint at or before `from_ms`.
    let idx = match checkpoints.binary_search_by(|c| c.wall_ms.cmp(&from_ms)) {
        Ok(i) => i,
        Err(0) => 0,
        Err(i) => i - 1,
    };
    checkpoints[idx].byte_offset
}

/// Parse only the records in `[from_ms, to_ms]` (0 = unbounded) using the
/// checkpoint index, then hand the bounded region to the authoritative loader.
fn load_region(
    path: &Path,
    header: Option<&str>,
    from_ms: u64,
    to_ms: u64,
) -> Result<SessionData, BridgeError> {
    let size = std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| BridgeError::new("io-error", e.to_string()))?;
    let idx = get_window_index(path, size)?;
    let start = start_offset_for(&idx.checkpoints, from_ms);
    let file =
        std::fs::File::open(path).map_err(|e| BridgeError::new("io-error", e.to_string()))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    reader
        .seek(SeekFrom::Start(start))
        .map_err(|e| BridgeError::new("io-error", e.to_string()))?;
    let mut text = String::new();
    // A mid-file region has no header of its own; prepend the real one so the
    // authoritative loader sees the recorded cadence. A region from offset 0
    // already contains the header.
    if start > 0
        && let Some(h) = header
    {
        text.push_str(h);
        text.push('\n');
    }
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| BridgeError::new("io-error", e.to_string()))?;
        if n == 0 {
            break;
        }
        let raw = String::from_utf8_lossy(&line);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let w = extract_wall_ms(trimmed).unwrap_or(0);
        if to_ms > 0 && w > to_ms {
            break;
        }
        if w == 0 || w >= from_ms {
            text.push_str(trimmed);
            text.push('\n');
        }
    }
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    session::load_session(&label, &text).map_err(|e| {
        BridgeError::new("session-corrupt", "cannot parse session window").with_detail(e)
    })
}

/// Interval from the session header (0 when absent).
pub(crate) fn session_interval_ms(path: &Path) -> Result<u64, BridgeError> {
    let (head, _tail, _size) = read_head_tail(path)?;
    Ok(header_json(&head)
        .and_then(|v| v.get("interval_ms").and_then(|m| m.num()))
        .unwrap_or(0.0)
        .max(0.0) as u64)
}

/// Parse only the records in `[from_ms, to_ms]` (0 = unbounded), prepending
/// the real header so the authoritative loader sees the session clock base.
/// Used by range analysis so it never materializes the whole recording.
pub(crate) fn load_region_data(
    path: &Path,
    from_ms: u64,
    to_ms: u64,
) -> Result<SessionData, BridgeError> {
    let (head, _tail, _size) = read_head_tail(path)?;
    let header = header_line(&head);
    load_region(path, header.as_deref(), from_ms, to_ms)
}

/// Load the authoritative whole session. Used only by whole-session analysis
/// (interesting-region discovery + event rail); bounded window access never
/// materializes the full recording.
pub(crate) fn load_full(path: &Path) -> Result<SessionData, BridgeError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        BridgeError::new("io-error", format!("cannot read {}", path.display()))
            .with_detail(e.to_string())
    })?;
    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    session::load_session(&label, &text).map_err(|e| {
        BridgeError::new("session-corrupt", format!("cannot parse {label}")).with_detail(e)
    })
}

/// Bounded window access. `from_ms`/`to_ms` of 0 are unbounded; `max_points`
/// of 0 uses [`MAX_WINDOW_POINTS`].
pub fn session_window_range(
    path: &Path,
    from_ms: u64,
    to_ms: u64,
    max_points: usize,
    pid: Option<u32>,
) -> Result<SessionWindow, BridgeError> {
    let (head, _tail, _size) = read_head_tail(path)?;
    let header = header_line(&head);
    let interval_ms = header_json(&head)
        .and_then(|v| v.get("interval_ms").and_then(|m| m.num()))
        .unwrap_or(0.0)
        .max(0.0) as u64;
    let s = load_region(path, header.as_deref(), from_ms, to_ms)?;
    let max = if max_points == 0 {
        MAX_WINDOW_POINTS
    } else {
        max_points.min(20_000)
    };
    let series = evidence::domain_series(&s, max, pid);
    let source_points = series.iter().map(|x| x.points.len()).max().unwrap_or(0);
    Ok(SessionWindow {
        path: path.to_string_lossy().to_string(),
        interval_ms,
        from_ms,
        to_ms,
        max_points: max,
        downsampled: source_points >= max,
        source_points,
        series,
    })
}

/// Convenience wrapper for tests and full-range callers.
#[cfg_attr(not(test), allow(dead_code))]
pub fn session_window(path: &Path) -> Result<SessionWindow, BridgeError> {
    session_window_range(path, 0, 0, MAX_WINDOW_POINTS, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pf-gui-sessions-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn existing_sessions_directory_wins() {
        let base = temp_dir("resolve-existing");
        std::fs::create_dir_all(base.join("sessions")).unwrap();
        assert_eq!(
            existing_sessions_dir(Some(&base), None),
            Some(base.join("sessions"))
        );
    }

    #[test]
    fn clean_install_has_no_workspace_sessions_directory() {
        let base = temp_dir("resolve-clean");
        // Nothing named `sessions` exists: the installed app must fall back to
        // the per-user data directory rather than writing next to the binary.
        assert_eq!(existing_sessions_dir(Some(&base), Some(&base)), None);
    }

    #[test]
    fn user_data_root_is_absolute() {
        assert!(user_data_root().is_absolute());
    }

    #[test]
    fn cargo_target_dirs_are_development_builds() {
        assert!(is_cargo_target_dir(Path::new("C:/repo/target/debug")));
        assert!(is_cargo_target_dir(Path::new("C:/repo/target/release")));
        assert!(!is_cargo_target_dir(Path::new(
            "C:/Users/u/AppData/Local/power-forensics"
        )));
        assert!(!is_cargo_target_dir(Path::new("C:/repo/target")));
    }

    /// A session with `n` battery samples at `step_ms`, plus a marker.
    fn write_session(path: &Path, n: u64, step_ms: u64) {
        let mut text =
            String::from("{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000}\n");
        for i in 0..n {
            text.push_str(&format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\
                 \"discharge_w\":{{\"v\":{},\"p\":\"measured\"}}}}\n",
                1000 + i * step_ms,
                i * step_ms,
                5.0 + (i % 7) as f64
            ));
        }
        text.push_str(
            "{\"type\":\"event\",\"wall_ms\":2000,\"mono_ms\":1000,\"kind\":\"marker\",\"detail\":\"m\"}\n",
        );
        text.push_str(
            "{\"type\":\"session_footer\",\"wall_ms\":60000,\"summary\":{\"samples\":1,\
             \"discharge_wh\":1.2,\"discharge_coverage_pct\":98.7,\"recovered\":false}}\n",
        );
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn window_index_cache_is_bounded() {
        let dir = temp_dir("cachebound");
        for i in 0..(MAX_WINDOW_INDEX_ENTRIES + 3) {
            let path = dir.join(format!("s{i}.jsonl"));
            write_session(&path, 3, 1000);
            let size = std::fs::metadata(&path).unwrap().len();
            let _ = get_window_index(&path, size).unwrap();
        }
        let len = window_index_cache().lock().map(|c| c.len()).unwrap_or(0);
        assert!(
            len <= MAX_WINDOW_INDEX_ENTRIES,
            "window index cache must stay bounded, got {len}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_returns_only_requested_interval() {
        let dir = temp_dir("window");
        let path = dir.join("s.jsonl");
        write_session(&path, 1000, 100); // wall 1000..100900
        let w = session_window_range(&path, 50_000, 60_000, 100, None).unwrap();
        let bat = w
            .series
            .iter()
            .find(|s| s.key == "battery_discharge_w")
            .unwrap();
        assert!(!bat.points.is_empty());
        for p in &bat.points {
            assert!(
                (50_000..=60_000).contains(&p.t_ms),
                "point {} outside window",
                p.t_ms
            );
        }
        // The window is far smaller than the file, so it was not fully parsed.
        assert!(bat.points.len() < 200, "got {} points", bat.points.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_downsampling_preserves_spikes_and_gaps() {
        let dir = temp_dir("spike");
        let path = dir.join("s.jsonl");
        let mut text =
            String::from("{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000}\n");
        for i in 0..1000u64 {
            let v = if i == 500 { 100.0 } else { 5.0 };
            let val = if i == 700 {
                "null".to_string()
            } else {
                format!("{v}")
            };
            let prov = if i == 700 { "unavailable" } else { "measured" };
            text.push_str(&format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\
                 \"discharge_w\":{{\"v\":{val},\"p\":\"{prov}\"}}}}\n",
                1000 + i * 1000,
                i * 1000
            ));
        }
        std::fs::write(&path, text).unwrap();
        let w = session_window_range(&path, 0, 0, 100, None).unwrap();
        let bat = w
            .series
            .iter()
            .find(|s| s.key == "battery_discharge_w")
            .unwrap();
        assert!(
            bat.points.len() <= 120,
            "bounded to max, got {}",
            bat.points.len()
        );
        assert!(
            bat.points.iter().any(|p| p.value == Some(100.0)),
            "the 100 W spike must survive downsampling"
        );
        assert!(
            bat.points.iter().any(|p| p.value.is_none()),
            "a gap must survive downsampling as a gap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn downsampling_keeps_a_provenance_transition() {
        use crate::dto::TimelinePoint;
        let points: Vec<TimelinePoint> = (0..100u64)
            .map(|i| TimelinePoint {
                t_ms: i * 1000,
                value: Some(6.0),
                provenance: if i < 50 { "measured" } else { "estimated" }.to_string(),
                quality: "fresh".to_string(),
            })
            .collect();
        let (out, downsampled) = evidence::downsample(points, 20);
        assert!(downsampled);
        // A provenance change at constant value must survive the bucket.
        assert!(out.iter().any(|p| p.provenance == "measured"), "{out:?}");
        assert!(out.iter().any(|p| p.provenance == "estimated"), "{out:?}");
    }

    #[test]
    fn active_session_outside_dir_is_rejected() {
        let dir = temp_dir("confine");
        let outside = temp_dir("confine-outside");
        let out_file = outside.join("x.jsonl");
        std::fs::write(&out_file, "{}").unwrap();
        let err = confine_active_session(&dir, &out_file.to_string_lossy()).unwrap_err();
        assert_eq!(err.code, "session-outside-dir");
        let inside = dir.join("in.jsonl");
        std::fs::write(&inside, "{}").unwrap();
        let ok = confine_active_session(&dir, &inside.to_string_lossy()).unwrap();
        assert!(ok.ends_with("in.jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
