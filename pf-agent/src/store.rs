//! Session store: batched, append-only JSONL.
//!
//! One line per record; a session is a header line, sample/event lines, and
//! a footer line. JSONL keeps writes O(1) and amortized-cheap (buffered +
//! periodic flush), which matters for observer effect. A compact binary
//! time-series + SQLite index is the planned next step once schemas freeze.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use pf_core::telemetry::escape_json;

/// Append-only buffered writer with an explicit durability contract.
///
/// **Loss bound on a hard kill** (process/console killed without reaching the
/// graceful path): at most `flush_every - 1` most-recent lines, or the
/// `BufWriter`'s 8 KiB capacity, whichever is reached first, may be lost.
/// `flush` (and a `footer`, which flushes) pushes the buffer to the OS; it
/// does **not** `fsync`, so an OS crash or power loss can additionally lose
/// everything since the last flush. Graceful shutdown (footer path) always
/// flushes in full.
pub struct SessionWriter {
    writer: BufWriter<File>,
    pub path: PathBuf,
    since_flush: usize,
    flush_every: usize,
    pub lines: u64,
}

impl SessionWriter {
    pub fn create(path: PathBuf, flush_every: usize) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        Ok(SessionWriter {
            writer: BufWriter::new(file),
            path,
            since_flush: 0,
            flush_every: flush_every.max(1),
            lines: 0,
        })
    }

    /// Push buffered bytes to the OS. Called on the periodic bound and on
    /// graceful shutdown. Does not fsync (see the loss bound above).
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()?;
        self.since_flush = 0;
        Ok(())
    }

    pub fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.lines += 1;
        self.since_flush += 1;
        if self.since_flush >= self.flush_every {
            self.flush()?;
        }
        Ok(())
    }

    pub fn header(
        &mut self,
        wall_ms: u64,
        tool_version: &str,
        interval_ms: u64,
        note: &str,
    ) -> std::io::Result<()> {
        self.write_line(&format!(
            "{{\"type\":\"session_header\",\"wall_ms\":{wall_ms},\
             \"tool\":\"power-forensics\",\"version\":\"{}\",\"format\":\"pf-jsonl-2\",\
             \"interval_ms\":{interval_ms},\"note\":\"{}\"}}",
            escape_json(tool_version),
            escape_json(note)
        ))
    }

    pub fn event(
        &mut self,
        wall_ms: u64,
        mono_ms: u64,
        kind: &str,
        detail: &str,
    ) -> std::io::Result<()> {
        self.write_line(&format!(
            "{{\"type\":\"event\",\"wall_ms\":{wall_ms},\"mono_ms\":{mono_ms},\
             \"kind\":\"{}\",\"detail\":\"{}\"}}",
            escape_json(kind),
            escape_json(detail)
        ))
    }

    pub fn footer(&mut self, wall_ms: u64, summary_json: &str) -> std::io::Result<()> {
        self.write_line(&format!(
            "{{\"type\":\"session_footer\",\"wall_ms\":{wall_ms},\
             \"lines\":{},\"summary\":{summary_json}}}",
            self.lines + 1
        ))?;
        self.flush()
    }
}

/// Estimated storage rate in MiB/hour for a session file. Returns None when
/// the duration is not positive (never invent a rate for a zero span).
pub fn estimate_mb_per_hour(bytes: u64, duration_s: f64) -> Option<f64> {
    if !duration_s.is_finite() || duration_s <= 0.0 {
        return None;
    }
    Some(bytes as f64 / duration_s * 3600.0 / 1_048_576.0)
}

/// Plan a storage-budget prune: given `(name, bytes, mtime_secs)` entries,
/// return the names to delete (oldest first) so the remainder fits
/// `budget_bytes`. Pure; the caller deletes (or dry-runs).
///
/// An unknown mtime (`None`, e.g. an unreadable file) is never treated as
/// oldest: such entries are excluded from both the plan and the total, so a
/// file whose age cannot be established is never selected for deletion and
/// cannot inflate the over-budget amount. The caller reports them.
pub fn plan_budget_prune(files: &[(String, u64, Option<u64>)], budget_bytes: u64) -> Vec<String> {
    let mut sorted: Vec<(&str, u64, u64)> = files
        .iter()
        .filter_map(|(name, bytes, mtime)| mtime.map(|m| (name.as_str(), *bytes, m)))
        .collect();
    sorted.sort_by_key(|(_, _, mtime)| *mtime);
    let total: u64 = sorted.iter().map(|(_, b, _)| *b).sum();
    let mut over = total.saturating_sub(budget_bytes);
    let mut victims = Vec::new();
    for (name, bytes, _) in sorted {
        if over == 0 {
            break;
        }
        victims.push(name.to_string());
        over = over.saturating_sub(bytes);
    }
    victims
}

/// Duration of a session file in seconds from its header/footer wall clocks.
/// Scans header + footer lines only (fast for long sessions).
pub fn session_duration_s(text: &str) -> Option<f64> {
    let mut header_wall: Option<u64> = None;
    let mut footer_wall: Option<u64> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = pf_core::json::parse(line) else {
            continue;
        };
        let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let wall = v.get("wall_ms").and_then(|m| m.num()).unwrap_or(0.0) as u64;
        if typ == "session_header" && header_wall.is_none() {
            header_wall = Some(wall);
        } else if typ == "session_footer" {
            footer_wall = Some(wall);
        }
    }
    match (header_wall, footer_wall) {
        (Some(h), Some(f)) if f > h => Some((f - h) as f64 / 1000.0),
        _ => None,
    }
}

/// Rebuild a session file missing its footer: returns the full text of a
/// COPY with a reconstructed `session_footer` appended (`recovered:true`).
///
/// Recovery is **line-granular** and independent of the strict whole-file
/// parser (`session::load_session` fails the entire file on one bad line, so
/// it cannot recover a torn write). Each line is parsed with the shared JSON
/// parser and kept only if it is a recognized record; scanning stops at the
/// first partial/corrupt line (the torn tail). The original `str` is never
/// mutated and the corrupt tail is discarded, so a torn footer cannot be
/// mistaken for a real one and no duplicate footer is ever emitted. Fails
/// when no header survives or a real footer is already present.
pub fn recover_session_text(original: &str, wall_now_ms: u64) -> Result<String, String> {
    let mut kept: Vec<&str> = Vec::new();
    let mut has_header = false;
    let mut last_wall: u64 = 0;
    // Effective cadence recorded by the header. Current writers record the
    // ACTUAL effective cadence (after preset resolution), so this matches the
    // live finalization's gap rule. Legacy headers written before that fix may
    // record the raw `--interval-ms` for preset sessions; the preset cannot be
    // recovered with certainty (it was never persisted), and guessing it from
    // sample timing would be derived evidence, so those old preset sessions
    // retain the historical ambiguity rather than being silently "corrected".
    let mut interval_ms: u64 = 1000;
    for line in original.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // First partial/corrupt line = torn tail: stop, discard the rest.
        let Ok(v) = pf_core::json::parse(trimmed) else {
            break;
        };
        let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let is_sample = v.get("collector").and_then(|c| c.as_str()).is_some();
        let is_event = typ == "event";
        if typ == "session_footer" {
            return Err("session already has a footer".to_string());
        }
        if !(is_sample || is_event || typ == "session_header") {
            // A syntactically valid but unrecognized line ends the salvageable
            // prefix; never carry it into a supposedly loadable session.
            break;
        }
        if typ == "session_header" {
            has_header = true;
            if let Some(iv) = v.get("interval_ms").and_then(|m| m.num())
                && iv >= 1.0
            {
                interval_ms = iv as u64;
            }
        }
        if let Some(w) = v.get("wall_ms").and_then(|m| m.num())
            && w > 0.0
        {
            last_wall = last_wall.max(w as u64);
        }
        kept.push(line);
    }
    if !has_header {
        return Err("no session_header: nothing to recover".to_string());
    }
    // Footer wall = last observed wall clock (what a clean stop would have
    // stamped), falling back to now.
    let footer_wall = if last_wall > 0 {
        last_wall
    } else {
        wall_now_ms
    };
    // Lightweight summary computed locally from the retained battery records,
    // using the SAME dual-clock discontinuity rules as the live footer
    // (`stats::classify_step` / `integrate_segmented_clocks`) so a recovered
    // session cannot tell a materially different story than a clean one:
    // coverage, unobserved duration and discontinuity counts are all reported.
    let mut times: Vec<f64> = Vec::new();
    let mut walls: Vec<f64> = Vec::new();
    let mut watts: Vec<Option<f64>> = Vec::new();
    let mut charges: Vec<Option<f64>> = Vec::new();
    for line in &kept {
        let Ok(v) = pf_core::json::parse(line.trim()) else {
            continue;
        };
        if v.get("collector").and_then(|c| c.as_str()) != Some("battery") {
            continue;
        }
        let mono = v.get("mono_ms").and_then(|m| m.num()).unwrap_or(0.0);
        let wall = v.get("wall_ms").and_then(|m| m.num()).unwrap_or(0.0);
        times.push(mono / 1000.0);
        walls.push(wall / 1000.0);
        watts.push(
            v.get("discharge_w")
                .and_then(|d| d.get("v"))
                .and_then(|x| x.num()),
        );
        charges.push(
            v.get("charge_w")
                .and_then(|d| d.get("v"))
                .and_then(|x| x.num()),
        );
    }
    let disch: Vec<f64> = watts.iter().filter_map(|w| *w).collect();
    let charge_n = charges.iter().filter(|c| c.is_some()).count();
    let gap = pf_core::stats::energy_max_gap_secs(interval_ms);
    let skew = pf_core::analysis::MAX_CLOCK_SKEW_SECS;
    let dseg = pf_core::stats::integrate_segmented_clocks(&times, &walls, &watts, gap, skew);
    let cseg = pf_core::stats::integrate_segmented_clocks(&times, &walls, &charges, gap, skew);
    let mut kinds = [0u64; 4];
    for d in pf_core::stats::discontinuities(&times, &walls, gap, skew) {
        kinds[d.kind.index()] += 1;
    }
    let kind_json = {
        let mut s = String::from("{");
        for (i, k) in pf_core::stats::DiscontinuityKind::ALL.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("\"{}\":{}", k.as_str(), kinds[k.index()]));
        }
        s.push('}');
        s
    };
    let med = pf_core::stats::median(&disch);
    let num = |o: Option<f64>, prec: usize| match o {
        Some(v) => format!("{v:.prec$}"),
        None => "null".to_string(),
    };
    let dwh = (dseg.covered_secs > 0.0).then_some(dseg.energy_wh);
    let cwh = (cseg.covered_secs > 0.0).then_some(cseg.energy_wh);
    let pct = |seg: &pf_core::stats::SegmentEnergy| {
        seg.coverage()
            .map(|c| format!("{:.1}", c * 100.0))
            .unwrap_or_else(|| "null".to_string())
    };
    let summary = format!(
        "{{\"samples\":{},\"discharge_n\":{},\"charge_n\":{charge_n},\
         \"discharge_median_w\":{},\"discharge_wh\":{},\"charge_wh\":{},\
         \"discharge_coverage_pct\":{},\"charge_coverage_pct\":{},\
         \"discharge_unknown_s\":{:.0},\"discharge_unobserved_s\":{:.0},\
         \"discharge_discontinuities\":{},\"discharge_discontinuity_kinds\":{kind_json},\
         \"recovered\":true}}",
        times.len(),
        disch.len(),
        num(med, 3),
        num(dwh, 4),
        num(cwh, 4),
        pct(&dseg),
        pct(&cseg),
        dseg.unknown_secs,
        dseg.unobserved_secs,
        dseg.discontinuities,
    );
    let mut out = kept.join("\n");
    out.push('\n');
    out.push_str(&format!(
        "{{\"type\":\"session_footer\",\"wall_ms\":{footer_wall},\"lines\":{},\
         \"recovered\":true,\"summary\":{summary}}}",
        kept.len() + 1
    ));
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_needs_positive_duration() {
        // 1 MiB over 1 hour => 1 MiB/h.
        assert_eq!(estimate_mb_per_hour(1_048_576, 3600.0), Some(1.0));
        assert_eq!(estimate_mb_per_hour(100, 0.0), None);
        assert_eq!(estimate_mb_per_hour(100, -5.0), None);
    }

    #[test]
    fn budget_prune_deletes_oldest_first() {
        let files = vec![
            ("new.jsonl".to_string(), 60, Some(300)),
            ("old.jsonl".to_string(), 60, Some(100)),
            ("mid.jsonl".to_string(), 60, Some(200)),
        ];
        // 180 total, budget 100 => drop oldest (old) -> 120, still over =>
        // drop mid too.
        assert_eq!(
            plan_budget_prune(&files, 100),
            vec!["old.jsonl".to_string(), "mid.jsonl".to_string()]
        );
        // Under budget => nothing.
        assert!(plan_budget_prune(&files, 1000).is_empty());
    }

    #[test]
    fn budget_prune_never_selects_unknown_mtime() {
        let files = vec![
            // Unknown age must not masquerade as oldest, even though it is
            // first and would be the first victim under the old 0 sentinel.
            ("unreadable.jsonl".to_string(), 999, None),
            ("old.jsonl".to_string(), 60, Some(100)),
            ("new.jsonl".to_string(), 60, Some(300)),
        ];
        // Known total is 120, budget 100 => only the oldest *known* file goes.
        assert_eq!(
            plan_budget_prune(&files, 100),
            vec!["old.jsonl".to_string()]
        );
        // Under budget (of the known files) => nothing, and the unreadable
        // entry never inflates the over-budget amount.
        assert!(plan_budget_prune(&files, 120).is_empty());
        // All-unknown is a no-op: never delete what cannot be aged.
        let all_unknown = vec![
            ("a.jsonl".to_string(), 10, None),
            ("b.jsonl".to_string(), 10, None),
        ];
        assert!(plan_budget_prune(&all_unknown, 1).is_empty());
    }

    #[test]
    fn duration_needs_header_and_footer() {
        let text = "{\"type\":\"session_header\",\"wall_ms\":1000}\n\
                    {\"type\":\"session_footer\",\"wall_ms\":61000,\"lines\":2,\"summary\":{}}\n";
        assert_eq!(session_duration_s(text), Some(60.0));
        assert_eq!(
            session_duration_s("{\"type\":\"session_header\",\"wall_ms\":1}\n"),
            None
        );
    }

    #[test]
    fn recover_appends_footer_copy() {
        let text = "{\"type\":\"session_header\",\"wall_ms\":1000}\n\
                    {\"collector\":\"battery\",\"wall_ms\":1000,\"mono_ms\":0,\
                    \"discharge_w\":{\"v\":6.0,\"p\":\"measured\"}}\n";
        let out = recover_session_text(text, 2000).unwrap();
        assert!(out.starts_with(text));
        assert!(out.contains("\"recovered\":true"));
        assert!(out.contains("session_footer"));
        // Reload: records preserved, footer present.
        let s = pf_core::session::load_session("r", &out).unwrap();
        assert_eq!(s.battery.len(), 1);
        assert!(s.footer.is_some());
        // Double recovery refused; headerless refused.
        assert!(recover_session_text(&out, 3000).is_err());
        assert!(recover_session_text("{}\n", 3000).is_err());
    }

    #[test]
    fn event_detail_escapes_control_and_unicode() {
        // Marker text is user-controlled; newlines/tabs/quotes must be escaped
        // so a marker cannot forge extra JSONL records, and Unicode must
        // round-trip through load.
        let dir = std::env::temp_dir().join(format!("pf-store-ev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ev.jsonl");
        let raw = "line1\nline2\ttab \"quote\" \u{00e9}\u{0394}";
        let mut w = SessionWriter::create(path.clone(), 1).unwrap();
        w.header(1, "0.1.0", 1000, "n").unwrap();
        w.event(2, 1, "marker", raw).unwrap();
        w.footer(3, "{}").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("\"type\":\"session_footer\"").count(), 1);
        // The escaped newline did not create an extra line.
        let s = pf_core::session::load_session("e", &text).unwrap();
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, "marker");
        assert_eq!(s.events[0].detail, raw);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recover_uses_dual_clock_and_reports_coverage() {
        // Three 1 s pairs at 6 W, then a suspend-like step: wall leaps 63 s
        // while monotonic advances only 1 s. The old mono-only summary would
        // bridge it; recovery must apply the live dual-clock rules and expose
        // coverage/discontinuity evidence.
        let mut text =
            String::from("{\"type\":\"session_header\",\"wall_ms\":1000,\"interval_ms\":1000}\n");
        for (wall, mono) in [(1000u64, 0u64), (2000, 1000), (3000, 2000), (66000, 3000)] {
            text.push_str(&format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{wall},\"mono_ms\":{mono},\
                 \"discharge_w\":{{\"v\":6.0,\"p\":\"measured\"}}}}\n"
            ));
        }
        let out = recover_session_text(&text, 70000).unwrap();
        let s = pf_core::session::load_session("r", &out).unwrap();
        let f = s.footer.as_ref().unwrap();
        let model = pf_core::stats::integrate_segmented_clocks(
            &[0.0, 1.0, 2.0, 3.0],
            &[1.0, 2.0, 3.0, 66.0],
            &[Some(6.0); 4],
            5.0,
            pf_core::analysis::MAX_CLOCK_SKEW_SECS,
        );
        let wh = f.get("discharge_wh").and_then(|v| v.num()).unwrap();
        assert!(
            (wh - model.energy_wh).abs() < 5e-4,
            "wh {wh} != {}",
            model.energy_wh
        );
        assert_eq!(
            f.get("discharge_discontinuities")
                .and_then(|v| v.num())
                .unwrap() as usize,
            model.discontinuities
        );
        assert_eq!(
            f.get("discharge_unknown_s").and_then(|v| v.num()).unwrap(),
            model.unknown_secs
        );
        let cov = f
            .get("discharge_coverage_pct")
            .and_then(|v| v.num())
            .unwrap();
        assert!(cov < 100.0, "wall jump must reduce coverage, got {cov}");
        // Mono-only would have integrated all 4 pairs; dual-clock refuses one.
        let mono_only =
            pf_core::stats::integrate_segmented(&[0.0, 1.0, 2.0, 3.0], &[Some(6.0); 4], 5.0);
        assert!(
            wh < mono_only.energy_wh,
            "recovered summary bridged the wall jump"
        );
    }

    #[test]
    fn recover_salvages_valid_prefix_before_torn_line() {
        // valid,valid,valid,then a torn write: every complete record must be
        // recovered and the partial line must not leak into the output (the
        // strict loader would fail the whole file on that last line).
        let text = "{\"type\":\"session_header\",\"wall_ms\":1000}\n\
                    {\"collector\":\"battery\",\"wall_ms\":1000,\"mono_ms\":0,\"discharge_w\":{\"v\":6.0,\"p\":\"measured\"}}\n\
                    {\"collector\":\"battery\",\"wall_ms\":2000,\"mono_ms\":1000,\"discharge_w\":{\"v\":6.1,\"p\":\"measured\"}}\n\
                    {\"collector\":\"battery\",\"wall_ms\":3000,\"mono_ms\":2000,\"dischar";
        let out = recover_session_text(text, 5000).unwrap();
        assert_eq!(out.matches("session_footer").count(), 1);
        let s = pf_core::session::load_session("r", &out).unwrap();
        assert_eq!(s.battery.len(), 2);
        assert!(s.footer.is_some());
        // Source is untouched (pure function over &str).
        assert!(text.ends_with("\"dischar"));
    }

    #[test]
    fn recover_drops_torn_footer_instead_of_refusing() {
        // A torn footer is not a real footer: it must be discarded and a
        // single valid footer appended, not treated as completeness.
        let text = "{\"type\":\"session_header\",\"wall_ms\":1000}\n\
                    {\"collector\":\"battery\",\"wall_ms\":1000,\"mono_ms\":0,\"discharge_w\":{\"v\":6.0,\"p\":\"measured\"}}\n\
                    {\"type\":\"session_footer\",\"wall_ms\":2000,\"lines\":3,\"summ";
        let out = recover_session_text(text, 9000).unwrap();
        assert_eq!(out.matches("session_footer").count(), 1);
        let s = pf_core::session::load_session("r", &out).unwrap();
        assert_eq!(s.battery.len(), 1);
        assert!(s.footer.is_some());
    }

    #[test]
    fn recover_refuses_complete_and_headerless() {
        let complete = "{\"type\":\"session_header\",\"wall_ms\":1000}\n\
                        {\"type\":\"session_footer\",\"wall_ms\":2000,\"lines\":2,\"summary\":{}}\n";
        assert!(recover_session_text(complete, 3000).is_err());
        assert!(recover_session_text("{\"collector\":\"battery\",\"mono_ms\":0}\n", 3000).is_err());
    }

    #[test]
    fn flush_bound_is_flush_every_minus_one() {
        let dir = std::env::temp_dir().join(format!("pf-store-flush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("flush.jsonl");
        let mut w = SessionWriter::create(path.clone(), 10).unwrap();
        for _ in 0..9 {
            w.write_line("x").unwrap();
        }
        // 9 lines buffered and nothing on disk: a hard kill here loses at
        // most flush_every-1 = 9 lines (the documented bound).
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        w.write_line("x").unwrap(); // 10th line triggers the periodic flush
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 10);
        // Graceful flush (also performed by `footer`) pushes any remainder.
        w.write_line("y").unwrap();
        w.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 11);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
