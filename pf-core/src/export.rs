//! Machine-readable exports that preserve evidence semantics.
//!
//! The raw session stays authoritative. Exports never invent a value: an
//! unavailable reading is written with an empty `value` cell plus its
//! structured `unavailable_kind`, `reason` and `provenance`, so a consumer can
//! never mistake absence for zero. Stale evidence stays labelled stale.
//!
//! Redaction reuses the existing backend engine ([`crate::analysis::redact_report_identifiers`])
//! and is strictly opt-in; the same token set the CLI uses is derived here from
//! the session model so the GUI cannot drift into a second implementation.

use std::io::{self, Write};

use crate::analysis::{self, RedactToken};
use crate::session::{self, RawRow, SessionData};

/// Stable schema version for the tidy tabular export. Bump on any column
/// change so downstream consumers can refuse silently-wrong parses.
pub const TIDY_SCHEMA_VERSION: u32 = 1;

/// Tidy long-format export columns. Every row carries its own provenance and
/// absence semantics; there is no implicit "empty means zero".
pub const TIDY_CSV_HEADER: &str = "mono_s,wall_ms,collector,metric,instance,value,unit,provenance,quality,unavailable_kind,reason,stale,session_id";

/// Sensitive tokens fed to `redact_report_identifiers`: process names
/// (as `process-N`), and the bare hardware/network/identity values that appear
/// as ordinary rendered values (GPU/display/network/USB/battery names, Wi-Fi
/// SSIDs, session label, current host/user). Shared by the CLI and GUI so both
/// redact identically.
pub fn session_identifiers(sessions: &[&SessionData]) -> Vec<RedactToken> {
    use RedactToken as T;
    let mut tokens: Vec<T> = Vec::new();
    let mut proc_i = 0usize;
    for s in sessions {
        for p in &s.procs {
            for e in &p.top {
                proc_i += 1;
                let ph = format!("process-{proc_i}");
                tokens.push(T::new(e.name.clone(), ph.clone()));
                tokens.push(T::new(format!("{}:{}", e.pid, e.name), ph));
            }
        }
        for g in &s.gpu {
            for a in &g.adapters {
                if !a.name.is_empty() {
                    tokens.push(T::new(a.name.clone(), "<gpu>"));
                }
                for (pid, _) in &a.top_pids {
                    tokens.push(T::new(format!("{}:{pid}", a.name), "<gpu>"));
                }
            }
        }
        for d in &s.display {
            for e in &d.displays {
                if !e.name.is_empty() {
                    tokens.push(T::new(e.name.clone(), "<display>"));
                }
            }
            for (inst, _) in &d.unmatched_sensors {
                if !inst.is_empty() {
                    tokens.push(T::new(inst.clone(), "<display>"));
                }
            }
        }
        for n in &s.net {
            for a in &n.adapters {
                for v in [&a.alias, &a.descr] {
                    if !v.is_empty() {
                        tokens.push(T::new(v.clone(), "<device>"));
                    }
                }
            }
            for (inst, _, _) in &n.throughput {
                if !inst.is_empty() {
                    tokens.push(T::new(inst.clone(), "<device>"));
                }
            }
            for w in &n.wifi {
                if !w.ssid.is_empty() {
                    tokens.push(T::new(w.ssid.clone(), "<ssid>"));
                }
                if !w.descr.is_empty() {
                    tokens.push(T::new(w.descr.clone(), "<device>"));
                }
            }
        }
        for u in &s.usb {
            for dev in &u.power_devices {
                if !dev.is_empty() {
                    tokens.push(T::new(dev.clone(), "<device>"));
                }
            }
        }
        for b in &s.battery_meta.batteries {
            for v in [&b.device_name, &b.mfg_name, &b.unique_id] {
                if let Some(v) = v
                    && !v.is_empty()
                {
                    tokens.push(T::new(v.clone(), "<device>"));
                }
            }
        }
        if let Some(scheme) = &s.policy.scheme_name
            && !scheme.is_empty()
            && !is_builtin_power_scheme(scheme)
        {
            tokens.push(T::new(scheme.clone(), "<scheme>"));
        }
        if !s.label.is_empty() {
            tokens.push(T::new(s.label.clone(), "<label>"));
        }
    }
    if let Ok(host) = std::env::var("COMPUTERNAME")
        && !host.is_empty()
    {
        tokens.push(T::new(host, "<host>"));
    }
    if let Ok(user) = std::env::var("USERNAME").or_else(|_| std::env::var("USER"))
        && !user.is_empty()
    {
        tokens.push(T::new(user, "<user>"));
    }
    tokens
}

/// Standard Windows power schemes are generic and stay readable; only custom
/// (user-named) schemes are treated as identity.
pub fn is_builtin_power_scheme(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "balanced" | "high performance" | "power saver" | "ultimate performance"
    )
}

/// Unit for a known (collector, metric) pair. Unknown metrics return `""`
/// (unknown), never a guessed unit.
pub fn metric_unit(collector: &str, metric: &str) -> &'static str {
    match collector {
        "battery" => match metric {
            "discharge_w" | "charge_w" => "W",
            "remaining_wh" => "Wh",
            "charge_pct" | "health" => "%",
            "rate_mw" => "mW",
            "ac" => "state",
            "temperature_raw" => "raw",
            _ => "",
        },
        "cpu" => match metric {
            "utility_pct" | "c3_pct" | "core.utility" => "%",
            "idle_breaks_s" => "s",
            "pkg_derived_w" | "pkg_power_w" => "W",
            "ctx_switches_s" => "/s",
            "core.freq_mhz" => "MHz",
            "core.performance" | "core.parking" => "state",
            _ => "",
        },
        "gpu" => match metric {
            "total_util_pct" | "util_3d" | "util_compute" | "util_decode" | "util_codec"
            | "util_copy" | "util_other" | "top_pid_util_pct" => "%",
            "mem_dedicated_mb" | "mem_shared_mb" => "MB",
            "engines_active" | "engines_seen" => "count",
            _ => "",
        },
        "proc" => match metric {
            "cpu_pct" => "%",
            _ => "",
        },
        "display" => match metric {
            "brightness_pct" => "%",
            "width" | "height" => "px",
            "freq_hz" => "Hz",
            "bpp" => "bpp",
            _ => "",
        },
        "net" => match metric {
            "rx_bps" | "tx_bps" | "total_rx_bps" | "total_tx_bps" => "bit/s",
            "wifi_signal_pct" => "%",
            _ => "",
        },
        "storage" => match metric {
            "disk_time_pct" => "%",
            "read_bps" | "write_bps" => "B/s",
            _ => "",
        },
        "usb" => match metric {
            "device_count" => "count",
            _ => "",
        },
        "self" => match metric {
            "cpu_pct" => "%",
            "ws_mb" | "priv_mb" => "MB",
            "io_read_b" | "io_write_b" => "B",
            "threads" => "count",
            "ctx_switches_s" => "/s",
            _ => "",
        },
        _ => "",
    }
}

/// Render one tidy row (with trailing newline). Absent values stay an empty
/// cell; the absence is fully described by the adjacent columns.
pub fn tidy_csv_row(row: &RawRow, session_id: &str) -> String {
    let value = row.value.map(|v| v.to_string()).unwrap_or_default();
    let kind = row.unavailable_kind.unwrap_or("");
    let stale = if row.is_stale() { "true" } else { "false" };
    format!(
        "{:.3},{},{},{},{},{},{},{},{},{},{},{},{}\n",
        row.mono_s,
        row.wall_ms,
        row.collector,
        session::format_csv_cell(&row.metric),
        session::format_csv_cell(&row.extra),
        value,
        metric_unit(row.collector, &row.metric),
        row.provenance,
        row.quality,
        kind,
        session::format_csv_cell(&row.reason),
        stale,
        session::format_csv_cell(session_id),
    )
}

/// Stream a tidy CSV to `w`, returning the number of data rows written. The
/// full text is never materialized. When `redact` is set every emitted line
/// goes through the shared backend redactor with the session-derived tokens.
pub fn write_tidy_csv<W: Write>(
    w: &mut W,
    s: &SessionData,
    session_id: &str,
    redact: bool,
) -> io::Result<usize> {
    // Column names are never identifying; the session id, which may be, is
    // folded into each data row so redaction reaches it too.
    w.write_all(TIDY_CSV_HEADER.as_bytes())?;
    w.write_all(b"\n")?;
    let tokens = redact.then(|| session_identifiers(&[s]));
    let mut rows = 0usize;
    for row in session::export_rows_iter(s) {
        let line = tidy_csv_row(&row, session_id);
        match &tokens {
            Some(tokens) => {
                w.write_all(analysis::redact_report_identifiers(&line, tokens).as_bytes())?
            }
            None => w.write_all(line.as_bytes())?,
        }
        rows += 1;
    }
    w.flush()?;
    Ok(rows)
}

/// Convenience wrapper returning the whole tidy CSV as a string (tests, small
/// sessions). Large exports should use [`write_tidy_csv`].
pub fn tidy_csv(s: &SessionData, session_id: &str, redact: bool) -> String {
    let mut out = Vec::new();
    let _ = write_tidy_csv(&mut out, s, session_id, redact);
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::BatteryPoint;
    use crate::telemetry::{ClockStamp, Telemetry, UnavailKind};

    fn stamp(ms: u64) -> ClockStamp {
        ClockStamp {
            wall_millis: ms,
            mono_millis: ms,
        }
    }

    #[test]
    fn absent_value_is_never_written_as_zero() {
        let mut s = SessionData::default();
        s.battery.push(BatteryPoint {
            t: 1.0,
            discharge: Telemetry::unavailable(
                UnavailKind::NotSampled,
                "not discharging (on AC or idle)",
                "battery",
                stamp(1000),
            ),
            ..Default::default()
        });
        let csv = tidy_csv(&s, "sess", false);
        let line = csv.lines().nth(1).unwrap();
        // value cell is empty, not "0"; absence metadata is present.
        assert!(line.contains(",discharge_w,,,W,unavailable,"), "{line}");
        assert!(line.contains("not-sampled"), "{line}");
        assert!(!line.contains(",0,"), "{line}");
    }

    #[test]
    fn stale_quality_is_retained() {
        let mut s = SessionData::default();
        let mut t = Telemetry::measured(5.0, "battery", stamp(1000));
        t.quality = crate::telemetry::SampleQuality::Stale;
        s.battery.push(BatteryPoint {
            t: 1.0,
            discharge: t,
            ..Default::default()
        });
        let csv = tidy_csv(&s, "sess", false);
        let line = csv.lines().nth(1).unwrap();
        assert!(line.contains(",5,W,measured,stale,"), "{line}");
        assert!(line.ends_with(",true,sess"), "{line}");
    }

    #[test]
    fn derived_provenance_is_preserved() {
        let mut s = SessionData::default();
        s.battery.push(BatteryPoint {
            t: 1.0,
            discharge: Telemetry::derived(7.0, "display-calib", stamp(1000)),
            ..Default::default()
        });
        let csv = tidy_csv(&s, "sess", false);
        let line = csv.lines().nth(1).unwrap();
        assert!(line.contains(",7,W,derived,"), "{line}");
    }

    #[test]
    fn csv_quoting_handles_commas_quotes_newlines_and_unicode() {
        let mut s = SessionData::default();
        s.battery.push(BatteryPoint {
            t: 1.0,
            discharge: Telemetry::measured(5.0, "battery", stamp(1000)),
            ..Default::default()
        });
        s.procs.push(crate::session::ProcPoint {
            t: 1.0,
            top: vec![crate::session::ProcEntry {
                pid: 42,
                name: "名, \"x\"\ny.exe".to_string(),
                cpu: Telemetry::measured(1.0, "proc", stamp(1000)),
                ..Default::default()
            }],
            ..Default::default()
        });
        let csv = tidy_csv(&s, "sess", false);
        // The instance cell is quoted because it holds a comma/newline, and the
        // embedded quote is doubled per RFC 4180.
        assert!(csv.contains("\"\"x\"\""), "{csv}");
        assert!(csv.contains("cpu_pct"), "{csv}");
        // Unicode survives unmodified.
        assert!(csv.contains("名"));
    }

    #[test]
    fn redaction_masks_process_names_and_label() {
        let mut s = SessionData {
            label: "MY-HOST".to_string(),
            ..Default::default()
        };
        s.battery.push(BatteryPoint {
            t: 1.0,
            discharge: Telemetry::measured(5.0, "battery", stamp(1000)),
            ..Default::default()
        });
        s.procs.push(crate::session::ProcPoint {
            t: 1.0,
            top: vec![crate::session::ProcEntry {
                pid: 42,
                name: "secret-app.exe".to_string(),
                cpu: Telemetry::measured(1.0, "proc", stamp(1000)),
                ..Default::default()
            }],
            ..Default::default()
        });
        let csv = tidy_csv(&s, "MY-HOST", true);
        assert!(!csv.contains("secret-app.exe"), "{csv}");
        assert!(csv.contains("process-1"), "{csv}");
    }

    #[test]
    fn unknown_metric_has_empty_unit() {
        assert_eq!(metric_unit("battery", "discharge_w"), "W");
        assert_eq!(metric_unit("cpu", "totals.something_else"), "");
    }
}
