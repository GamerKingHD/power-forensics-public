//! Analyze workspace bridge: maps pure `pf_core::analyze` results and the
//! authoritative session model onto wire DTOs.
//!
//! No forensic calculation happens here. Range quality, energy integration,
//! change detection, process aggregation and correlation all live in
//! `pf-core`; this module only adapts them to the GUI and bounds the amount of
//! raw session it ever materializes.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use pf_core::analyze as core;
use pf_core::session::SessionData;

use crate::dto::{
    AnalyzeOverviewDto, CategoryChangeDto, ChangeFactDto, CollectorSampleCount, CorrelationDto,
    DomainAvailabilityDto, DomainStatDto, InterestingRegionDto, MetricStatDto, ProcessAnalysisDto,
    ProcessEntryDto, RangeAnalysisDto, RangeEnergyDto, RangeQualityDto, SessionStats,
};
use crate::evidence;
use crate::index::Fingerprint;
use crate::sessions;

/// Maximum context window placed on either side of a selected range. This
/// bounds the region parsed for before/after comparison.
const MAX_CONTEXT_MS: u64 = 5 * 60_000;
const MIN_CONTEXT_MS: u64 = 10_000;

fn context_ms(from_ms: u64, to_ms: u64) -> u64 {
    to_ms
        .saturating_sub(from_ms)
        .clamp(MIN_CONTEXT_MS, MAX_CONTEXT_MS)
}

/// Compute a range analysis by parsing only the interval plus its context
/// windows. The selected range is the only analytical subject; the extra
/// before/after context exists solely for comparison.
pub fn range_analysis(
    path: &Path,
    interval_ms: u64,
    from_ms: u64,
    to_ms: u64,
) -> Result<RangeAnalysisDto, crate::dto::BridgeError> {
    let (from_ms, to_ms) = (from_ms.min(to_ms), to_ms.max(from_ms));
    let ctx = context_ms(from_ms, to_ms);
    let load_from = from_ms.saturating_sub(ctx);
    let load_to = to_ms.saturating_add(ctx);
    let s = sessions::load_region_data(path, load_from, load_to)?;
    let a = core::analyze_range(&s, interval_ms, from_ms, to_ms);
    Ok(RangeAnalysisDto {
        path: path.to_string_lossy().to_string(),
        from_ms: a.from_ms,
        to_ms: a.to_ms,
        duration_s: a.duration_s,
        quality: RangeQualityDto {
            span_s: a.quality.span_s,
            covered_s: a.quality.covered_s,
            unknown_s: a.quality.unknown_s,
            unobserved_s: a.quality.unobserved_s,
            coverage: a.quality.coverage,
            discontinuities: a.quality.discontinuities,
            timeouts: a.quality.timeouts,
            recoveries: a.quality.recoveries,
            errors: a.quality.errors,
            events: a.quality.events,
            markers: a.quality.markers,
            samples: a.quality.samples,
            collector_counts: a
                .quality
                .collector_counts
                .into_iter()
                .map(|(name, samples)| CollectorSampleCount {
                    name: name.to_string(),
                    samples: samples as u64,
                })
                .collect(),
            stale_collectors: a
                .quality
                .stale_collectors
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
        },
        energy: RangeEnergyDto {
            discharge_wh: a.energy.discharge_wh,
            charge_wh: a.energy.charge_wh,
            covered_s: a.energy.covered_s,
            unknown_s: a.energy.unknown_s,
            unobserved_s: a.energy.unobserved_s,
            discontinuities: a.energy.discontinuities,
            crosses_discontinuity: a.energy.crosses_discontinuity,
            discharge_present: a.energy.discharge_present,
            charge_present: a.energy.charge_present,
        },
        domains: a
            .domains
            .into_iter()
            .map(|d| DomainStatDto {
                domain: d.domain.to_string(),
                available: d.available,
                metrics: d
                    .metrics
                    .into_iter()
                    .map(|m| MetricStatDto {
                        key: m.key.to_string(),
                        label: m.label.to_string(),
                        unit: m.unit.to_string(),
                        n: m.stat.n,
                        known: m.stat.known,
                        median: m.stat.median,
                        min: m.stat.min,
                        max: m.stat.max,
                        p10: m.stat.p10,
                        p90: m.stat.p90,
                        provenance: m.provenance.to_string(),
                    })
                    .collect(),
            })
            .collect(),
        processes: ProcessAnalysisDto {
            ticks: a.processes.ticks,
            entries: a
                .processes
                .entries
                .into_iter()
                .map(|p| ProcessEntryDto {
                    pid: p.pid,
                    name: p.name,
                    ppid: p.ppid,
                    cpu_median_pct: p.cpu_median_pct,
                    cpu_max_pct: p.cpu_max_pct,
                    presence: p.presence,
                    ticks: p.ticks,
                    first_seen_ms: p.first_seen_ms,
                    last_seen_ms: p.last_seen_ms,
                    start_unix_ms: p.start_unix_ms,
                })
                .collect(),
            total_threads: a.processes.total_threads,
            inaccessible: a.processes.inaccessible,
            truncated: a.processes.truncated,
            incomplete: a.processes.incomplete,
            note: a.processes.note,
        },
        changes: a
            .changes
            .into_iter()
            .map(|c| ChangeFactDto {
                domain: c.domain.to_string(),
                key: c.key.to_string(),
                label: c.label.to_string(),
                unit: c.unit.to_string(),
                reference: c.reference,
                during: c.during,
                after: c.after,
                delta: c.delta,
                direction: c.direction.to_string(),
                confidence: c.confidence.to_string(),
                basis: c.basis.to_string(),
                n_before: c.n_before,
                n_during: c.n_during,
                n_after: c.n_after,
            })
            .collect(),
        categorical: a
            .categorical
            .into_iter()
            .map(|c| CategoryChangeDto {
                domain: c.domain.to_string(),
                label: c.label.to_string(),
                before: c.before,
                during: c.during,
                after: c.after,
                basis: c.basis.to_string(),
            })
            .collect(),
        correlations: a
            .correlations
            .into_iter()
            .map(|c| CorrelationDto {
                x: c.x.to_string(),
                y: c.y.to_string(),
                label: c.label,
                n: c.n,
                effective_n: c.effective_n,
                r: c.r,
                coverage: c.coverage,
                strength: c.strength.to_string(),
                note: c.note,
            })
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// Whole-session overview
// ---------------------------------------------------------------------------

struct CacheEntry {
    path: PathBuf,
    bytes: u64,
    mtime_s: u64,
    /// Active calibration revision the entry was computed under. Calibration
    /// can shape derived/estimated evidence, so a change must invalidate the
    /// cached analysis rather than serve a stale interpretation.
    calibration_id: String,
    value: AnalyzeOverviewDto,
}

fn overview_cache() -> &'static Mutex<Option<CacheEntry>> {
    static CACHE: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Cache-validity predicate. A cached analysis may only be reused when the file
/// fingerprint AND the active calibration identity both match, so changing the
/// active calibration can never return a result computed under the previous
/// revision.
fn cache_hit(entry: &CacheEntry, path: &Path, fp: Fingerprint, calibration_id: &str) -> bool {
    entry.path == path
        && entry.bytes == fp.bytes
        && entry.mtime_s == fp.mtime_s
        && entry.calibration_id == calibration_id
}

/// Whole-session statistics derived from the authoritative model: record
/// counts, distinct ticks, clock bounds, event tallies and effective rate.
fn session_stats(s: &SessionData) -> SessionStats {
    use std::collections::{BTreeMap, HashSet};
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut ticks: HashSet<u64> = HashSet::new();
    let mut first: Option<u64> = None;
    let mut last: Option<u64> = None;
    let mut note = |wall: u64| {
        first = Some(first.map_or(wall, |f| f.min(wall)));
        last = Some(last.map_or(wall, |l| l.max(wall)));
        if ticks.len() < 2_000_000 {
            ticks.insert(wall);
        }
    };
    for p in &s.battery {
        *counts.entry("battery").or_insert(0) += 1;
        note(evidence::series_wall(&p.discharge, s, p.t));
    }
    for p in &s.cpu {
        *counts.entry("cpu").or_insert(0) += 1;
        note(evidence::series_wall(&p.utility, s, p.t));
    }
    for p in &s.gpu {
        *counts.entry("gpu").or_insert(0) += 1;
        note(evidence::mono_wall(s, p.t));
    }
    for p in &s.procs {
        *counts.entry("proc").or_insert(0) += 1;
        note(evidence::mono_wall(s, p.t));
    }
    for p in &s.display {
        *counts.entry("display").or_insert(0) += 1;
        note(evidence::series_wall(&p.brightness, s, p.t));
    }
    for p in &s.net {
        *counts.entry("net").or_insert(0) += 1;
        note(evidence::series_wall(&p.total_rx, s, p.t));
    }
    for p in &s.storage {
        *counts.entry("storage").or_insert(0) += 1;
        note(evidence::series_wall(&p.disk_time, s, p.t));
    }
    for p in &s.usb {
        *counts.entry("usb").or_insert(0) += 1;
        note(evidence::mono_wall(s, p.t));
    }
    for p in &s.selfmon {
        *counts.entry("self").or_insert(0) += 1;
        note(evidence::series_wall(&p.cpu_pct, s, p.t));
    }
    for (t, _) in &s.policy_history {
        *counts.entry("os_power").or_insert(0) += 1;
        note(evidence::mono_wall(s, *t));
    }
    let total_samples: u64 = counts.values().sum();
    let duration_s = match (first, last) {
        (Some(a), Some(b)) if b > a => Some((b - a) as f64 / 1000.0),
        _ => None,
    };
    // Effective rate of the busiest collector (per-tick timestamps differ
    // slightly between independent collectors).
    let max_samples = counts.values().copied().max().unwrap_or(0);
    let observed_hz = duration_s
        .filter(|d| *d > 0.0)
        .map(|d| max_samples as f64 / d);
    let (mut events, mut markers, mut timeouts, mut recoveries, mut errors) = (0, 0, 0, 0, 0);
    for e in &s.events {
        events += 1;
        let k = e.kind.to_ascii_lowercase();
        if k == "marker" {
            markers += 1;
        } else if k.contains("timeout") {
            timeouts += 1;
        } else if k.contains("recover") {
            recoveries += 1;
        } else if k.contains("error") || k.contains("fail") {
            errors += 1;
        }
    }
    SessionStats {
        total_samples,
        ticks: ticks.len() as u64,
        collector_samples: counts
            .into_iter()
            .map(|(name, samples)| CollectorSampleCount {
                name: name.to_string(),
                samples,
            })
            .collect(),
        first_sample_wall_ms: first,
        last_sample_wall_ms: last,
        observed_hz,
        events,
        markers,
        timeouts,
        recoveries,
        errors,
        discontinuities: None,
        duration_s,
    }
}

fn observed_collectors(s: &SessionData) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |name: &str, has: bool| {
        if has {
            out.push(name.to_string());
        }
    };
    push("battery", !s.battery.is_empty());
    push("cpu", !s.cpu.is_empty());
    push("gpu", !s.gpu.is_empty());
    push("proc", !s.procs.is_empty());
    push("display", !s.display.is_empty());
    push("net", !s.net.is_empty());
    push("storage", !s.storage.is_empty());
    push("usb", !s.usb.is_empty());
    push("self", !s.selfmon.is_empty());
    push("os_power", !s.policy_history.is_empty());
    out
}

fn domain_availability(s: &SessionData, from_ms: u64, to_ms: u64) -> Vec<DomainAvailabilityDto> {
    core::domain_breakdown(s, from_ms, to_ms)
        .into_iter()
        .map(|d| DomainAvailabilityDto {
            domain: d.domain.to_string(),
            available: d.available,
            reason: (!d.available).then(|| format!("no {} evidence was recorded", d.domain)),
        })
        .collect()
}

/// Whole-session overview used to open the Analyze workspace. Cached by file
/// fingerprint, so navigating away and back never re-scans the JSONL.
pub fn overview(
    path: &Path,
    caps: &crate::dto::CapabilityReport,
) -> Result<AnalyzeOverviewDto, crate::dto::BridgeError> {
    overview_keyed(path, caps, "")
}

/// Like [`overview`], but binds the cache entry to a calibration identity so a
/// cached analysis is never returned after the active calibration changes.
pub fn overview_keyed(
    path: &Path,
    caps: &crate::dto::CapabilityReport,
    calibration_id: &str,
) -> Result<AnalyzeOverviewDto, crate::dto::BridgeError> {
    let fp = Fingerprint::of(path).unwrap_or(Fingerprint {
        bytes: 0,
        mtime_s: 0,
    });
    if let Ok(cache) = overview_cache().lock()
        && let Some(entry) = cache.as_ref()
        && cache_hit(entry, path, fp, calibration_id)
    {
        return Ok(entry.value.clone());
    }

    let s = sessions::load_full(path)?;
    let interval_ms = sessions::session_interval_ms(path)?;
    let (start, end) = core::session_span_ms(&s);
    let (head, tail, _size) = sessions::read_head_tail(path)?;
    let header = sessions::header_json(&head);
    let footer = sessions::footer_line(&tail).and_then(|l| pf_core::json::parse(&l).ok());
    let label = path
        .file_name()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();
    let note = header
        .as_ref()
        .and_then(|v| v.get("note"))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let fsum = evidence::footer_summary(footer.as_ref().and_then(|v| v.get("summary")));
    let observed = observed_collectors(&s);
    let stats = session_stats(&s);
    let marker_count = stats.markers;

    let value = AnalyzeOverviewDto {
        path: path.to_string_lossy().to_string(),
        label,
        note,
        status: if footer.is_some() { "ok" } else { "incomplete" }.to_string(),
        recovered: fsum.recovered,
        start_wall_ms: start,
        end_wall_ms: end,
        duration_s: if end > start {
            (end - start) as f64 / 1000.0
        } else {
            0.0
        },
        interval_ms,
        stats,
        collectors: evidence::collectors_view(
            &s,
            Some(observed.as_slice()),
            interval_ms,
            caps,
            end,
        ),
        events: evidence::events_view(&s, usize::MAX),
        regions: core::interesting_regions(&s, interval_ms)
            .into_iter()
            .map(|r| InterestingRegionDto {
                kind: r.kind.to_string(),
                domain: r.domain.to_string(),
                label: r.label,
                start_ms: r.start_ms,
                end_ms: r.end_ms,
                score: r.score,
                detail: r.detail,
            })
            .collect(),
        domains: domain_availability(&s, start, end),
        coverage_pct: fsum.discharge_coverage_pct,
        discontinuities: fsum.discharge_discontinuities,
        markers: marker_count,
        series: evidence::domain_series(&s, 1000, None),
    };
    if let Ok(mut cache) = overview_cache().lock() {
        *cache = Some(CacheEntry {
            path: path.to_path_buf(),
            bytes: fp.bytes,
            mtime_s: fp.mtime_s,
            calibration_id: calibration_id.to_string(),
            value: value.clone(),
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pf-gui-analyze-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 60 s steady 6 W discharge + cpu, with a spike, a marker and a footer.
    fn write_session(path: &std::path::Path) {
        let mut text = String::from(
            "{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000,\"note\":\"t\"}\n",
        );
        for i in 0..60u64 {
            let w = if i == 30 { 20.0 } else { 6.0 };
            text.push_str(&format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\
                 \"discharge_w\":{{\"v\":{w},\"p\":\"measured\"}},\"charge_pct\":{{\"v\":50,\"p\":\"measured\"}}}}\n",
                1000 + i * 1000,
                i * 1000
            ));
            text.push_str(&format!(
                "{{\"collector\":\"cpu\",\"wall_ms\":{},\"mono_ms\":{},\
                 \"totals\":{{\"utility\":{{\"v\":10,\"p\":\"measured\"}}}}}}\n",
                1000 + i * 1000,
                i * 1000
            ));
        }
        text.push_str("{\"type\":\"event\",\"wall_ms\":31000,\"mono_ms\":30000,\"kind\":\"marker\",\"detail\":\"spike\"}\n");
        text.push_str("{\"type\":\"session_footer\",\"wall_ms\":61000,\"summary\":{\"samples\":60,\"discharge_wh\":0.1,\"discharge_coverage_pct\":99.0,\"discharge_discontinuities\":0,\"recovered\":false}}\n");
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn range_analysis_uses_core_energy_and_swaps_bounds() {
        let dir = temp_dir("range");
        let path = dir.join("s.jsonl");
        write_session(&path);
        // Reversed bounds must be normalized, not rejected.
        let r = range_analysis(&path, 1000, 61_000, 1_000).unwrap();
        assert_eq!(r.from_ms, 1_000);
        assert_eq!(r.to_ms, 61_000);
        // Full-range discharge energy equals the whole-session integration.
        let s = sessions::load_full(&path).unwrap();
        let reference = core::range_energy(&s, 1000, 1000, 61_000);
        assert!((r.energy.discharge_wh.unwrap() - reference.discharge_wh.unwrap()).abs() < 1e-9);
        assert_eq!(r.quality.discontinuities, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overview_reports_whole_session_counts_events_and_regions() {
        let dir = temp_dir("overview");
        let path = dir.join("s.jsonl");
        write_session(&path);
        let caps = crate::capabilities::classify("{\"collectors\":[]}", false).unwrap();
        let o = overview(&path, &caps).unwrap();
        assert_eq!(o.stats.total_samples, 120);
        assert_eq!(o.stats.markers, 1);
        assert_eq!(o.stats.events, 1);
        assert_eq!(o.stats.first_sample_wall_ms, Some(1000));
        assert_eq!(o.stats.last_sample_wall_ms, Some(60000));
        // 60 ticks over 59 s.
        assert!((o.stats.observed_hz.unwrap() - 60.0 / 59.0).abs() < 1e-9);
        assert!(o.events.iter().any(|e| e.kind == "marker"));
        assert!(
            o.regions
                .iter()
                .any(|r| r.kind == "peak" || r.kind == "sustained_increase"),
            "{:?}",
            o.regions
        );
        assert!(!o.series.is_empty());
        // A missing GPU is reported as an unavailable domain, not a failure.
        let gpu = o.domains.iter().find(|d| d.domain == "gpu").unwrap();
        assert!(!gpu.available);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Latency harness against a real recording. Run explicitly:
    /// `PF_PERF_SESSION=<file> cargo test -p pf-gui --lib perf -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_real_session() {
        let path = std::env::var("PF_PERF_SESSION").unwrap_or_else(|_| {
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../sessions/phase1-baseline.jsonl"
            )
            .to_string()
        });
        let path = std::path::PathBuf::from(path);
        if !path.exists() {
            eprintln!("perf session not found: {}", path.display());
            return;
        }
        let caps = crate::capabilities::classify("{\"collectors\":[]}", false).unwrap();
        let t = std::time::Instant::now();
        let o = overview(&path, &caps).unwrap();
        let open = t.elapsed();
        println!(
            "open/overview: {:?} (samples={}, regions={}, events={}, series_pts={})",
            open,
            o.stats.total_samples,
            o.regions.len(),
            o.events.len(),
            o.series.iter().map(|s| s.points.len()).sum::<usize>()
        );
        let mid = o.start_wall_ms + (o.end_wall_ms - o.start_wall_ms) / 2;
        let t = std::time::Instant::now();
        let _ =
            sessions::session_window_range(&path, mid - 15_000, mid + 15_000, 1600, None).unwrap();
        println!("change window (30s, cold index): {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let _ =
            sessions::session_window_range(&path, mid - 5_000, mid + 5_000, 1600, None).unwrap();
        println!("change window (10s, warm index): {:?}", t.elapsed());
        let t = std::time::Instant::now();
        let r = range_analysis(&path, o.interval_ms, mid - 15_000, mid + 15_000).unwrap();
        println!(
            "select range (30s): {:?} (changes={}, correlations={})",
            t.elapsed(),
            r.changes.len(),
            r.correlations.len()
        );
        let t = std::time::Instant::now();
        let _ = sessions::session_window_range(&path, mid - 15_000, mid + 15_000, 1600, Some(4))
            .unwrap();
        println!("select process window: {:?}", t.elapsed());
        // A second overview must hit the fingerprint cache.
        let t = std::time::Instant::now();
        let _ = overview(&path, &caps).unwrap();
        println!("re-open (cached): {:?}", t.elapsed());
    }

    /// Opens every real session in `sessions/` and reports what the analyzer
    /// found. Run: `cargo test -p pf-gui --lib survey_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn survey_real_sessions() {
        let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../sessions"));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("no sessions dir at {}", dir.display());
            return;
        };
        let caps = crate::capabilities::classify("{\"collectors\":[]}", false).unwrap();
        let mut files: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
            .collect();
        files.sort();
        for path in files {
            match overview(&path, &caps) {
                Ok(o) => {
                    let region_kinds: Vec<&str> =
                        o.regions.iter().map(|r| r.kind.as_str()).collect();
                    let domains: Vec<&str> = o
                        .domains
                        .iter()
                        .filter(|d| d.available)
                        .map(|d| d.domain.as_str())
                        .collect();
                    println!(
                        "{} [{}] dur={:.0}s samples={} ticks={} events={} regions={:?} domains={:?}",
                        path.file_name().unwrap().to_string_lossy(),
                        o.status,
                        o.duration_s,
                        o.stats.total_samples,
                        o.stats.ticks,
                        o.events.len(),
                        region_kinds,
                        domains
                    );
                }
                Err(e) => println!(
                    "{} -> ERROR {}: {}",
                    path.file_name().unwrap().to_string_lossy(),
                    e.code,
                    e.message
                ),
            }
        }
    }

    #[test]
    fn overview_is_cached_by_fingerprint() {
        let dir = temp_dir("cache");
        let path = dir.join("s.jsonl");
        write_session(&path);
        let caps = crate::capabilities::classify("{\"collectors\":[]}", false).unwrap();
        let a = overview(&path, &caps).unwrap();
        let b = overview(&path, &caps).unwrap();
        assert_eq!(a.stats.total_samples, b.stats.total_samples);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_key_includes_calibration_identity() {
        let entry = CacheEntry {
            path: PathBuf::from("s.jsonl"),
            bytes: 10,
            mtime_s: 5,
            calibration_id: "display_power#1".to_string(),
            value: AnalyzeOverviewDto::default(),
        };
        let fp = Fingerprint {
            bytes: 10,
            mtime_s: 5,
        };
        // Same file, same calibration -> reusable.
        assert!(cache_hit(
            &entry,
            Path::new("s.jsonl"),
            fp,
            "display_power#1"
        ));
        // Same file, different calibration -> must NOT be reused.
        assert!(!cache_hit(
            &entry,
            Path::new("s.jsonl"),
            fp,
            "display_power#2"
        ));
        // Different file -> must NOT be reused.
        assert!(!cache_hit(
            &entry,
            Path::new("t.jsonl"),
            fp,
            "display_power#1"
        ));
    }
}
