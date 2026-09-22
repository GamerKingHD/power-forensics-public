//! Derived, disposable session index.
//!
//! The JSONL session files are authoritative. This cache exists so that
//! browsing a directory of long recordings does not re-scan every file on
//! every request. An entry is reused only when its fingerprint (file size +
//! mtime) still matches; otherwise it is rebuilt from the JSONL. The cache is
//! never a correctness dependency: a missing, corrupt, outdated or partly
//! invalid index is simply rebuilt or ignored.
//!
//! Contents per file: header/footer summary, counts, marker count, collector
//! set, coverage, discontinuities and the on-disk fingerprint. It stores no
//! sample values, so it can never become a second source of forensic truth.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::dto::{CollectorSampleCount, SessionListEntry, SessionStats};

/// Bump when the entry shape changes; a mismatch forces a full rebuild.
pub const INDEX_SCHEMA: u32 = 2;

/// Files larger than this are not stream-scanned for markers/collectors during
/// indexing; those fields stay unknown (`None` / empty) rather than being
/// guessed. The footer summary is still read from the tail.
pub const SCAN_MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIndex {
    pub schema: u32,
    pub dir: String,
    pub entries: Vec<SessionListEntry>,
}

impl SessionIndex {
    pub fn empty(dir: &Path) -> Self {
        SessionIndex {
            schema: INDEX_SCHEMA,
            dir: dir.to_string_lossy().to_string(),
            entries: Vec::new(),
        }
    }

    /// Load the index if it is present, schema-compatible and readable.
    /// Any failure yields `None` so the caller rebuilds; corrupt data is never
    /// partially trusted.
    pub fn load(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(index_path(dir)).ok()?;
        let parsed: SessionIndex = serde_json::from_str(&text).ok()?;
        if parsed.schema != INDEX_SCHEMA {
            return None;
        }
        Some(parsed)
    }

    /// Persist best-effort via a temp file + rename so a crash cannot leave a
    /// half-written index in place. Write errors are ignored: the index is a
    /// pure optimization.
    pub fn save(&self, dir: &Path) {
        let Ok(text) = serde_json::to_string(self) else {
            return;
        };
        let target = index_path(dir);
        let tmp = target.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &target);
        }
    }
}

pub fn index_path(dir: &Path) -> PathBuf {
    dir.join(".pf-gui-index.json")
}

/// A file's identity for cache validation. A recording that is still being
/// appended to changes size (and usually mtime), so it always rebuilds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub bytes: u64,
    pub mtime_s: u64,
}

impl Fingerprint {
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime_s = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some(Fingerprint {
            bytes: meta.len(),
            mtime_s,
        })
    }
}

/// Whole-session summary streamed once during indexing. `None` (too large or
/// unreadable) means unknown: no count is ever guessed.
pub struct ScanDetails {
    pub markers: u64,
    pub collectors: Vec<String>,
    pub stats: SessionStats,
}

/// Count records, events and clock bounds, streamed once. Uses tolerant
/// substring probes against the stable JSONL field names instead of parsing
/// every record, so a multi-hour file costs one linear pass and no full
/// materialization. Returns `None` when the file is too large to scan.
pub fn scan_details(path: &Path, bytes: u64) -> Option<ScanDetails> {
    if bytes > SCAN_MAX_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut markers: u64 = 0;
    let mut events: u64 = 0;
    let mut timeouts: u64 = 0;
    let mut recoveries: u64 = 0;
    let mut errors: u64 = 0;
    let mut total_samples: u64 = 0;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut first_wall: Option<u64> = None;
    let mut last_wall: Option<u64> = None;
    // Distinct record timestamps approximate the effective tick rate. Capped
    // so a pathological file cannot grow this set without bound.
    let mut ticks: HashSet<u64> = HashSet::new();
    const TICK_CAP: usize = 2_000_000;
    for line in BufReader::with_capacity(1 << 16, file).lines() {
        let Ok(line) = line else { break };
        let kind = str_field(&line, "\"type\"");
        if kind.as_deref() == Some("event") {
            events += 1;
            match str_field(&line, "\"kind\"").unwrap_or_default().as_str() {
                "marker" => markers += 1,
                k if k.contains("timeout") => timeouts += 1,
                k if k.contains("recover") => recoveries += 1,
                k if k.contains("error") || k.contains("fail") => errors += 1,
                _ => {}
            }
            continue;
        }
        let Some(collector) = str_field(&line, "\"collector\"") else {
            continue;
        };
        if collector.is_empty() {
            continue;
        }
        total_samples += 1;
        *counts.entry(collector).or_insert(0) += 1;
        if let Some(w) = num_field(&line, "\"wall_ms\"") {
            first_wall = Some(first_wall.map_or(w, |f| f.min(w)));
            last_wall = Some(last_wall.map_or(w, |l| l.max(w)));
            if ticks.len() < TICK_CAP {
                ticks.insert(w);
            }
        }
    }
    let duration_s = match (first_wall, last_wall) {
        (Some(a), Some(b)) if b > a => Some((b - a) as f64 / 1000.0),
        _ => None,
    };
    let tick_count = ticks.len() as u64;
    // Effective rate of the busiest collector: per-tick timestamps differ
    // slightly between independent collectors, so "distinct timestamps" alone
    // would overstate the recording cadence.
    let max_samples = counts.values().copied().max().unwrap_or(0);
    let observed_hz = duration_s
        .filter(|d| *d > 0.0)
        .map(|d| max_samples as f64 / d);
    let collectors: BTreeSet<String> = counts.keys().cloned().collect();
    Some(ScanDetails {
        markers,
        collectors: collectors.into_iter().collect(),
        stats: SessionStats {
            total_samples,
            ticks: tick_count,
            collector_samples: counts
                .into_iter()
                .map(|(name, samples)| CollectorSampleCount { name, samples })
                .collect(),
            first_sample_wall_ms: first_wall,
            last_sample_wall_ms: last_wall,
            observed_hz,
            events,
            markers,
            timeouts,
            recoveries,
            errors,
            discontinuities: None, // filled from the footer by the caller
            duration_s,
        },
    })
}

/// Extract a string field value, tolerant of optional whitespace around `:`
/// and the opening quote (fixtures and real recordings differ). Returns the
/// first occurrence only.
fn str_field(line: &str, key: &str) -> Option<String> {
    let start = line.find(key)? + key.len();
    let rest = line[start..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extract a non-negative integer field value, whitespace-tolerant.
fn num_field(line: &str, key: &str) -> Option<u64> {
    let start = line.find(key)? + key.len();
    let rest = line[start..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-gui-index-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_counts_markers_and_collectors() {
        let dir = temp_dir("scan");
        let path = dir.join("s.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session_header\",\"wall_ms\":1}\n\
             {\"collector\":\"battery\",\"wall_ms\":1,\"mono_ms\":0}\n\
             {\"collector\":\"cpu\",\"wall_ms\":2,\"mono_ms\":1}\n\
             {\"type\":\"event\",\"wall_ms\":3,\"mono_ms\":2,\"kind\":\"marker\",\"detail\":\"x\"}\n\
             {\"collector\":\"battery\",\"wall_ms\":4,\"mono_ms\":3}\n",
        )
        .unwrap();
        let scan = scan_details(&path, std::fs::metadata(&path).unwrap().len()).unwrap();
        assert_eq!(scan.markers, 1);
        // "battery" must be reported once even though it appears twice.
        assert_eq!(
            scan.collectors,
            vec!["battery".to_string(), "cpu".to_string()]
        );
        assert_eq!(scan.stats.total_samples, 3);
        assert_eq!(scan.stats.ticks, 3);
        assert_eq!(scan.stats.events, 1);
        assert_eq!(scan.stats.markers, 1);
        assert_eq!(scan.stats.first_sample_wall_ms, Some(1));
        assert_eq!(scan.stats.last_sample_wall_ms, Some(4));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_tolerates_whitespace_and_counts_event_kinds() {
        // Fixtures use `"key": value` spacing; real recordings use `"key":value`.
        let dir = temp_dir("scan-space");
        let path = dir.join("s.jsonl");
        std::fs::write(
            &path,
            "{\"type\": \"session_header\", \"wall_ms\": 1}\n\
             {\"collector\": \"battery\", \"wall_ms\": 1000, \"mono_ms\": 0}\n\
             {\"type\": \"event\", \"wall_ms\": 2000, \"mono_ms\": 1000, \"kind\": \"collector_timeout\", \"detail\": \"x\"}\n\
             {\"type\": \"event\", \"wall_ms\": 3000, \"mono_ms\": 2000, \"kind\": \"collector_recovered\", \"detail\": \"x\"}\n",
        )
        .unwrap();
        let scan = scan_details(&path, std::fs::metadata(&path).unwrap().len()).unwrap();
        assert_eq!(scan.stats.events, 2);
        assert_eq!(scan.stats.timeouts, 1);
        assert_eq!(scan.stats.recoveries, 1);
        assert_eq!(scan.stats.markers, 0);
        assert_eq!(scan.stats.total_samples, 1);
        // A single tick has no measurable span: duration stays unknown.
        assert_eq!(scan.stats.duration_s, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_index_is_ignored_and_rebuilt() {
        let dir = temp_dir("corrupt");
        std::fs::write(index_path(&dir), "{not json").unwrap();
        assert!(SessionIndex::load(&dir).is_none());
        let index = SessionIndex::empty(&dir);
        index.save(&dir);
        let loaded = SessionIndex::load(&dir).unwrap();
        assert_eq!(loaded.schema, INDEX_SCHEMA);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_mismatch_forces_rebuild() {
        let dir = temp_dir("schema");
        let mut index = SessionIndex::empty(&dir);
        index.schema = INDEX_SCHEMA + 99;
        std::fs::write(index_path(&dir), serde_json::to_string(&index).unwrap()).unwrap();
        assert!(
            SessionIndex::load(&dir).is_none(),
            "stale schema must not be trusted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
