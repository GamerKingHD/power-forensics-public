//! Observer-effect benchmark: controlled A/B measurement of the
//! profiler's own cost.
//!
//! Phase A records battery + self — a low-load proxy for "profiler not
//! running" while still capturing discharge and, via self, the phase-A
//! baseline the primary observer metric (B-A self-CPU delta) requires. It is
//! a PROXY, not a true
//! profiler-off baseline: the profiler binary is still running in phase A,
//! so any cost that does not scale with collector count (base process,
//! writer thread) is present in both phases and cancels out. A true
//! profiler-off idle number requires external measurement (profiler not
//! running at all). Phase B records the full normal set. Both phases run
//! back-to-back via the normal monitor path, so the comparison uses the
//! same experimental machinery as any other A/B test. The primary observer
//! metric is the self-CPU delta (GetProcessTimes(self) medians, phase B
//! minus phase A); verdicts are evaluated against the engineering
//! targets — and when the effect is below resolution, the report says so
//! instead of printing a precise-looking zero.

use pf_agent::MonitorOpts;
use pf_agent::monitor;
use pf_core::analysis;
use pf_core::session::SessionData;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Warn => "WARN",
            Verdict::Fail => "FAIL",
            Verdict::Unknown => "UNKNOWN",
        }
    }
}

pub struct OverheadVerdict {
    pub label: String,
    pub verdict: Verdict,
    pub detail: String,
}

/// Evaluate one target. Pure and tested; thresholds are the engineering
/// targets from the plan, not measured achievements. `own_cpu_delta` is the
/// observer-effect CPU metric (phase B minus phase A), not a full-phase
/// median; `discharge_spread_rel` is dimensionless (p90-p10)/median.
pub fn evaluate(
    own_cpu_delta: Option<f64>,
    mb_per_day_full: Option<f64>,
    discharge_delta_w: Option<f64>,
    discharge_spread_rel: Option<f64>,
) -> Vec<OverheadVerdict> {
    let mut out = Vec::new();
    out.push(match own_cpu_delta {
        Some(c) if c < 0.5 => OverheadVerdict {
            label: "profiler CPU <0.5%".to_string(),
            verdict: Verdict::Pass,
            detail: format!("own CPU delta {c:+.2}%"),
        },
        Some(c) => OverheadVerdict {
            label: "profiler CPU <0.5%".to_string(),
            verdict: Verdict::Fail,
            detail: format!("own CPU delta {c:+.2}% exceeds target"),
        },
        None => OverheadVerdict {
            label: "profiler CPU <0.5%".to_string(),
            verdict: Verdict::Unknown,
            detail: "no self samples on both phases (A and B needed to form a delta)".to_string(),
        },
    });
    out.push(match mb_per_day_full {
        Some(m) if m < 250.0 => OverheadVerdict {
            label: "storage <250 MB/day".to_string(),
            verdict: Verdict::Pass,
            detail: format!("{m:.0} MB/day extrapolated from full session"),
        },
        Some(m) => OverheadVerdict {
            label: "storage <250 MB/day".to_string(),
            verdict: Verdict::Fail,
            detail: format!(
                "{m:.0} MB/day exceeds target; use --preset low or archive aggressively"
            ),
        },
        None => OverheadVerdict {
            label: "storage <250 MB/day".to_string(),
            verdict: Verdict::Unknown,
            detail: "session size unknown".to_string(),
        },
    });
    out.push(match (discharge_delta_w, discharge_spread_rel) {
        (Some(d), Some(spread)) if d.abs() < 0.25 && spread < 1.0 => OverheadVerdict {
            label: "incremental power <0.25 W".to_string(),
            verdict: Verdict::Pass,
            detail: format!("Δ{d:+.2} W within target band (relative spread {spread:.2})"),
        },
        (Some(d), _) if d.abs() < 0.25 => OverheadVerdict {
            label: "incremental power <0.25 W".to_string(),
            verdict: Verdict::Warn,
            detail: format!("Δ{d:+.2} W within target band but noisy; rerun to confirm"),
        },
        (Some(d), _) => OverheadVerdict {
            label: "incremental power <0.25 W".to_string(),
            verdict: Verdict::Fail,
            detail: format!("Δ{d:+.2} W exceeds target; profile the collectors"),
        },
        (None, _) => OverheadVerdict {
            label: "incremental power <0.25 W".to_string(),
            verdict: Verdict::Unknown,
            detail: "no discharge data on both sides (on AC?); needs unplugged sessions"
                .to_string(),
        },
    });
    out
}

fn median(vals: &[f64]) -> Option<f64> {
    pf_core::stats::median(vals)
}

/// Median of a selfmon field across a session.
fn self_med(s: &SessionData, f: fn(&pf_core::session::SelfPoint) -> f64) -> Option<f64> {
    let vals: Vec<f64> = s.selfmon.iter().map(f).collect();
    median(&vals)
}

/// Observer-effect CPU metric: phase B minus phase A. None unless both
/// phases recorded self samples — a full-phase median alone is not a delta
/// and must not be passed off as incremental profiler cost.
fn self_cpu_delta(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(b - a),
        _ => None,
    }
}

/// The two benchmark phases. Phase A must record `self` so the primary
/// observer metric (B-A self-CPU delta) is measurable; battery stays for the
/// discharge proxy. Phase A remains a proxy, not a true profiler-off run.
fn overhead_phases() -> [(&'static str, &'static str, Option<String>, Option<String>); 2] {
    [
        (
            "a",
            "baseline (battery + self)",
            Some("battery,self".to_string()),
            None,
        ),
        ("b", "normal (all collectors)", None, None),
    ]
}

pub fn cmd_overhead(args: &[String]) -> i32 {
    // Phase D opt-in: manual profiler-off sandwich. Branched before the
    // default battery-vs-full benchmark so the default path stays untouched.
    if args.iter().any(|a| a == "--off-missing") {
        return cmd_off_missing(args);
    }
    let mut phase_secs = 120u64;
    let mut interval_ms = 1000u64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--phase" => {
                i += 1;
                phase_secs = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &u64| (15..=900).contains(v))
                    .unwrap_or(0);
                if phase_secs == 0 {
                    eprintln!("error: invalid --phase (15..900 seconds)");
                    return 2;
                }
            }
            "--interval-ms" => {
                i += 1;
                interval_ms = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &u64| (100..=60_000).contains(v))
                    .unwrap_or(0);
                if interval_ms == 0 {
                    eprintln!("error: invalid --interval-ms (100..60000)");
                    return 2;
                }
            }
            other => {
                eprintln!("error: bad overhead arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    if std::fs::create_dir_all("sessions").is_err() {
        eprintln!("error: cannot create sessions dir");
        return 2;
    }
    // Phase A: battery + self (baseline). Phase B: normal, everything.
    let phases = overhead_phases();
    let mut files = Vec::new();
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    for (tag, label, collectors, preset) in phases {
        println!("--- overhead phase {tag}: {label} ({phase_secs}s) ---");
        let out = format!("sessions/overhead-{tag}-{run_id}.jsonl");
        let code = monitor::cmd_monitor(MonitorOpts {
            interval_ms,
            samples: Some((phase_secs * 1000 / interval_ms).max(1)),
            out: Some(out.clone()),
            note: format!("overhead benchmark phase {tag}: {label}"),
            helper: None,
            helper_every: 60,
            collectors,
            preset,
        });
        if code != 0 {
            eprintln!("error: phase {tag} recording failed");
            return 2;
        }
        files.push(out);
    }
    let load = |f: &str| {
        std::fs::read_to_string(f)
            .map_err(|e| format!("cannot read {f}: {e}"))
            .and_then(|t| {
                let label = f.to_string();
                pf_core::session::load_session(&label, &t)
            })
    };
    let (sa, sb) = match (load(&files[0]), load(&files[1])) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    // Battery comparison reuses the experimental machinery.
    let cmp = analysis::compare_sessions(&sa, &sb);
    println!("\n{}", analysis::render_comparison(&cmp));
    // Self-telemetry: own CPU/memory/IO in both phases.
    // The self-CPU delta (phase B minus phase A) is the PRIMARY observer
    // metric: it is measured directly via GetProcessTimes(self), unlike the
    // discharge delta which the battery sensor may not resolve. Phase A now
    // records battery + self, so the delta is measurable; if a phase still
    // produced no self samples the delta is honestly UNKNOWN (a full-phase
    // median is not an incremental cost).
    let cpu_a = self_med(&sa, |p| p.cpu_pct.val().unwrap_or(f64::NAN)).filter(|v| v.is_finite());
    let cpu_b = self_med(&sb, |p| p.cpu_pct.val().unwrap_or(f64::NAN)).filter(|v| v.is_finite());
    let cpu_delta = self_cpu_delta(cpu_a, cpu_b);
    let ws_b = self_med(&sb, |p| p.ws_mb.val().unwrap_or(f64::NAN)).filter(|v| v.is_finite());
    let io_b = sb
        .selfmon
        .iter()
        .filter_map(|p| p.io_write_b.val())
        .next_back()
        .unwrap_or(0.0)
        - sb.selfmon
            .iter()
            .filter_map(|p| p.io_write_b.val())
            .next()
            .unwrap_or(0.0);
    println!("profiler self-telemetry (phase B, normal):");
    println!(
        "  own CPU median: {}",
        cpu_b
            .map(|v| format!("{v:.2}%"))
            .unwrap_or("n/a".to_string())
    );
    println!(
        "  own working set median: {}",
        ws_b.map(|v| format!("{v:.1} MB"))
            .unwrap_or("n/a".to_string())
    );
    println!("  own IO written during phase B: {:.1} KB", io_b / 1024.0);
    // Storage rate from the full phase.
    let mb_per_day = std::fs::metadata(&files[1]).ok().map(|m| {
        let secs = sb
            .battery
            .iter()
            .map(|p| p.t)
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), t| {
                (lo.min(t), hi.max(t))
            });
        let dur = (secs.1 - secs.0).max(1.0);
        m.len() as f64 / (1024.0 * 1024.0) / (dur / 86400.0)
    });
    let rel_spread = |vals: &[f64]| {
        analysis::PowerSummary::of(vals).and_then(|s| {
            if s.median.abs() > f64::EPSILON {
                Some((s.p90 - s.p10) / s.median)
            } else {
                None
            }
        })
    };
    let va: Vec<f64> = sa
        .battery
        .iter()
        .filter_map(|p| p.discharge.val())
        .collect();
    let vb: Vec<f64> = sb
        .battery
        .iter()
        .filter_map(|p| p.discharge.val())
        .collect();
    let spread = match (rel_spread(&va), rel_spread(&vb)) {
        (Some(a), Some(b)) => Some(a.max(b)),
        _ => None,
    };
    println!("\nOBSERVER-EFFECT VERDICT (targets are engineering goals, not claims):");
    for v in evaluate(cpu_delta, mb_per_day, cmp.delta_w, spread) {
        println!("  [{}] {} — {}", v.verdict.as_str(), v.label, v.detail);
    }
    // Primary observer metric, stated separately from the pass/fail band.
    match (cpu_a, cpu_b) {
        (Some(a), Some(b)) => println!(
            "self-CPU delta (primary observer metric): {b:.2}% - {a:.2}% = {:+.2}%",
            b - a
        ),
        (None, Some(b)) => println!(
            "self-CPU delta (primary observer metric): phase B median {b:.2}% (phase A recorded no self samples)"
        ),
        _ => println!("self-CPU delta (primary observer metric): n/a (no self samples)"),
    }
    println!(
        "method: back-to-back phases on one machine; rerun if the system was active (check CPU/C3 lines above)"
    );
    println!(
        "note: phase A is a low-load (battery+self) proxy for profiler-off, NOT a true idle baseline — true profiler-off requires external measurement (profiler not running)"
    );
    0
}

// ================= Phase D: manual profiler-off protocol =================
// Opt-in `--off-missing` mode: a manual true-profiler-off sandwich. The
// agent cannot record while it is stopped, so the off window between two
// agent-on sessions has no telemetry BY DESIGN. The off-window drain rate
// is endpoint math only: (remaining_wh at A-end minus remaining_wh at
// B-start) over the operator-measured off duration. Honest, low-resolution,
// and explicit about its confounders.

/// Target band for the on-vs-off delta (engineering goal, not a claim).
const OFF_TARGET_W: f64 = 0.25;
/// Display resolution: a delta below half a centiwatt would render as a
/// precise-looking "0.00 W" — report it as below resolution instead.
const OFF_RESOLUTION_W: f64 = 0.005;

pub struct OffMissingResult {
    pub on_a_w: Option<f64>,
    pub on_b_w: Option<f64>,
    pub rem_a_end_wh: Option<f64>,
    pub rem_b_start_wh: Option<f64>,
    pub off_w: Option<f64>,
    /// Why `off_w` is unavailable, when it is (e.g. net charging in the gap).
    pub off_w_reason: Option<String>,
    pub off_minutes: f64,
    pub off_minutes_defaulted: bool,
    pub delta_w: Option<f64>,
    pub spread: Option<f64>,
    pub verdict: Verdict,
    pub detail: String,
}

/// Median discharge of one agent-on phase (its own drain-rate proxy).
fn on_drain_rate(s: &SessionData) -> Option<f64> {
    let vals: Vec<f64> = s.battery.iter().filter_map(|p| p.discharge.val()).collect();
    median(&vals)
}

/// Endpoint drain across the agent-off gap. Err when an endpoint is missing
/// (no telemetry in the gap by design), the window is unusable, or the
/// endpoints show the battery charged: a negative (charging) drain is not a
/// drain and would inflate the on-vs-off delta.
pub fn off_drain_rate(
    rem_a_end_wh: Option<f64>,
    rem_b_start_wh: Option<f64>,
    off_hours: f64,
) -> Result<f64, &'static str> {
    match (rem_a_end_wh, rem_b_start_wh) {
        (Some(a), Some(b))
            if off_hours > 0.0 && off_hours.is_finite() && a.is_finite() && b.is_finite() =>
        {
            let drain = (a - b) / off_hours;
            if drain < 0.0 {
                Err(
                    "off-window endpoints show net charging (remaining increased); drain rate not usable",
                )
            } else {
                Ok(drain)
            }
        }
        _ => Err("off-window endpoints missing remaining_wh (no telemetry in the gap by design)"),
    }
}

/// Relative spread (p90-p10)/median of one discharge series.
fn discharge_spread(vals: &[f64]) -> Option<f64> {
    analysis::PowerSummary::of(vals).and_then(|s| {
        if s.median.abs() > f64::EPSILON {
            Some((s.p90 - s.p10) / s.median)
        } else {
            None
        }
    })
}

fn session_duration_secs(s: &SessionData) -> f64 {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for p in &s.battery {
        lo = lo.min(p.t);
        hi = hi.max(p.t);
    }
    if hi > lo && lo.is_finite() && hi.is_finite() {
        hi - lo
    } else {
        0.0
    }
}

/// Verdict over the on-vs-off delta. Mirrors `evaluate()`'s incremental-power
/// band: in-band + quiet -> Pass, in-band + noisy -> Warn, out-of-band ->
/// Fail, no data -> Unknown (never a precise zero below resolution).
pub fn evaluate_off_missing(
    on_a_w: Option<f64>,
    on_b_w: Option<f64>,
    off_w: Option<f64>,
    off_reason: Option<&str>,
    spread: Option<f64>,
) -> (Verdict, String) {
    let on = match (on_a_w, on_b_w) {
        (Some(a), Some(b)) => Some((a + b) / 2.0),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    let delta = match (on, off_w) {
        (Some(o), Some(f)) if o.is_finite() && f.is_finite() => Some(o - f),
        _ => None,
    };
    match (delta, spread) {
        (Some(d), Some(sp)) if d.abs() < OFF_TARGET_W && sp < 1.0 => {
            if d.abs() < OFF_RESOLUTION_W {
                (
                    Verdict::Pass,
                    format!(
                        "on-vs-off delta below resolution (within <0.25 W band; relative spread {sp:.2})"
                    ),
                )
            } else {
                (
                    Verdict::Pass,
                    format!("on-vs-off Δ{d:+.2} W within target band (relative spread {sp:.2})"),
                )
            }
        }
        (Some(d), _) if d.abs() < OFF_TARGET_W => {
            if d.abs() < OFF_RESOLUTION_W {
                (
                    Verdict::Warn,
                    "on-vs-off delta below resolution but noisy; rerun to confirm".to_string(),
                )
            } else {
                (
                    Verdict::Warn,
                    format!("on-vs-off Δ{d:+.2} W within target band but noisy; rerun to confirm"),
                )
            }
        }
        (Some(d), _) => {
            if d.abs() < OFF_RESOLUTION_W {
                // Unreachable (|d| >= target here); kept so a sub-resolution
                // delta can never render as a precise "+0.00 W".
                (
                    Verdict::Fail,
                    "on-vs-off delta below resolution".to_string(),
                )
            } else {
                (
                    Verdict::Fail,
                    format!("on-vs-off Δ{d:+.2} W exceeds target; profile the collectors"),
                )
            }
        }
        (None, _) if on.is_some() => (
            Verdict::Unknown,
            off_reason
                .unwrap_or(
                    "off-window endpoints missing remaining_wh (no telemetry in the gap by design); cannot compute off drain rate",
                )
                .to_string(),
        ),
        (None, _) => (
            Verdict::Unknown,
            "no discharge data on both sides (on AC?); needs unplugged sessions".to_string(),
        ),
    }
}

/// Full off-missing math over two loaded sessions. Pure and tested.
pub fn analyze_off_missing(a: &SessionData, b: &SessionData, off_minutes: f64) -> OffMissingResult {
    let on_a_w = on_drain_rate(a);
    let on_b_w = on_drain_rate(b);
    let rem_a_end_wh = a
        .battery
        .iter()
        .filter_map(|p| p.remaining_wh.val())
        .next_back();
    let rem_b_start_wh = b.battery.iter().filter_map(|p| p.remaining_wh.val()).next();
    let off_hours = off_minutes / 60.0;
    let (off_w, off_w_reason) = match off_drain_rate(rem_a_end_wh, rem_b_start_wh, off_hours) {
        Ok(w) => (Some(w), None),
        Err(reason) => (None, Some(reason.to_string())),
    };
    let va: Vec<f64> = a.battery.iter().filter_map(|p| p.discharge.val()).collect();
    let vb: Vec<f64> = b.battery.iter().filter_map(|p| p.discharge.val()).collect();
    let spread = match (discharge_spread(&va), discharge_spread(&vb)) {
        (Some(x), Some(y)) => Some(x.max(y)),
        _ => None,
    };
    let (verdict, detail) =
        evaluate_off_missing(on_a_w, on_b_w, off_w, off_w_reason.as_deref(), spread);
    let delta_w = match (on_a_w, on_b_w, off_w) {
        (Some(a), Some(b), Some(f)) => Some((a + b) / 2.0 - f),
        (Some(a), None, Some(f)) => Some(a - f),
        (None, Some(b), Some(f)) => Some(b - f),
        _ => None,
    };
    OffMissingResult {
        on_a_w,
        on_b_w,
        rem_a_end_wh,
        rem_b_start_wh,
        off_w,
        off_w_reason,
        off_minutes,
        off_minutes_defaulted: false,
        delta_w,
        spread,
        verdict,
        detail,
    }
}

fn cmd_off_missing(args: &[String]) -> i32 {
    let mut files: Vec<String> = Vec::new();
    let mut off_minutes: Option<f64> = None;
    let mut seen_flag = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--off-missing" => {
                if seen_flag {
                    eprintln!("error: duplicate --off-missing");
                    return 2;
                }
                seen_flag = true;
            }
            "--off-minutes" => {
                i += 1;
                let v: Option<f64> = args.get(i).and_then(|s| s.parse().ok());
                match v {
                    Some(n) if n.is_finite() && n > 0.0 && n <= 1440.0 => off_minutes = Some(n),
                    _ => {
                        eprintln!("error: invalid --off-minutes (positive minutes, max 1440)");
                        return 2;
                    }
                }
            }
            other if !other.starts_with("--") => files.push(other.to_string()),
            other => {
                eprintln!("error: bad overhead arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    if files.len() != 2 {
        eprintln!("error: overhead --off-missing needs exactly two session files (A B)");
        return 2;
    }
    println!("Phase D manual profiler-off protocol (--off-missing):");
    println!("  (1) idle 10min agent-on → session A file;");
    println!(
        "  (2) stop agent, idle same duration untouched (same AC state, same power plan, same screen state);"
    );
    println!("  (3) start agent, idle 10min → session B file.");
    println!("usage: overhead --off-missing A.jsonl B.jsonl [--off-minutes N]");
    let load = |f: &str| {
        std::fs::read_to_string(f)
            .map_err(|e| format!("cannot read {f}: {e}"))
            .and_then(|t| {
                let label = f.to_string();
                pf_core::session::load_session(&label, &t)
            })
    };
    let (sa, sb) = match (load(&files[0]), load(&files[1])) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    // --off-minutes is the operator-measured agent-OFF window between A-end
    // and B-start, used only for the drain-rate math narrative. Without it,
    // assume the gap equaled the on-phase duration.
    let (off_min, defaulted) = match off_minutes {
        Some(n) => (n, false),
        None => {
            let mut d = session_duration_secs(&sa) / 60.0;
            if d.is_nan() || d <= 0.0 {
                d = session_duration_secs(&sb) / 60.0;
            }
            if d.is_nan() || d <= 0.0 {
                d = 10.0;
            }
            (d, true)
        }
    };
    let mut r = analyze_off_missing(&sa, &sb, off_min);
    r.off_minutes_defaulted = defaulted;
    // A/B capture quality for the on-phases reuses the experimental machinery.
    let cmp = analysis::compare_sessions(&sa, &sb);
    println!("\n{}", analysis::render_comparison(&cmp));
    println!("OFF-WINDOW DRAIN (endpoint math; the gap itself has no telemetry by design):");
    match (r.on_a_w, r.on_b_w) {
        (Some(a), Some(b)) => println!(
            "  [measured] on-phase drain medians: A {a:.2} W, B {b:.2} W (avg {:.2} W)",
            (a + b) / 2.0
        ),
        (Some(a), None) => {
            println!("  [measured] on-phase drain median: A {a:.2} W (B has no discharge data)")
        }
        (None, Some(b)) => {
            println!("  [measured] on-phase drain median: B {b:.2} W (A has no discharge data)")
        }
        (None, None) => println!(
            "  [unavailable] on-phase drain: no discharge data on both sides (on AC?); needs unplugged sessions"
        ),
    }
    match (r.rem_a_end_wh, r.rem_b_start_wh, r.off_w) {
        (Some(a), Some(b), Some(f)) => println!(
            "  [derived] off-window drain: ({a:.3} - {b:.3} Wh) / {:.3} h = {f:.2} W (off window {:.1} min{})",
            r.off_minutes / 60.0,
            r.off_minutes,
            if defaulted {
                ", defaulted: assumed equal to phase duration"
            } else {
                ""
            }
        ),
        _ => println!(
            "  [unavailable] off-window drain: {}",
            r.off_w_reason.as_deref().unwrap_or(
                "needs remaining_wh at A-end and B-start (no telemetry in the gap by design)"
            )
        ),
    }
    match (r.delta_w, r.spread) {
        (Some(d), _) if d.abs() < OFF_RESOLUTION_W => {
            println!("  [derived] on-vs-off delta: below resolution (within <0.25 W band)")
        }
        (Some(d), Some(sp)) => println!(
            "  [derived] on-vs-off delta: {d:+.2} W (on-phase avg minus off-window drain; relative spread {sp:.2})"
        ),
        (Some(d), None) => println!(
            "  [derived] on-vs-off delta: {d:+.2} W (on-phase avg minus off-window drain; spread n/a)"
        ),
        (None, _) => println!(
            "  [unavailable] on-vs-off delta: cannot compare without on-phase and off-window rates"
        ),
    }
    println!("[confounder] agent off window has no telemetry (by design)");
    println!("[confounder] thermal/SOC drift uncontrolled — keep idle steady");
    println!(
        "[confounder] AC state must match (same AC state, same power plan, same screen state)"
    );
    println!(
        "off-window coverage: n/a (gap unobserved by design; A/B capture quality above applies to the on-phases only)"
    );
    println!("\nOFF-MISSING VERDICT (target is an engineering goal, not a claim):");
    println!(
        "  [{}] incremental power <0.25 W — {}",
        r.verdict.as_str(),
        r.detail
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_thresholds() {
        let v = evaluate(Some(0.2), Some(100.0), Some(0.1), Some(0.2));
        assert!(v.iter().all(|x| x.verdict == Verdict::Pass));
        let v = evaluate(Some(2.0), Some(500.0), Some(1.5), Some(0.2));
        assert!(v.iter().all(|x| x.verdict == Verdict::Fail));
        let v = evaluate(None, None, None, None);
        assert!(v.iter().all(|x| x.verdict == Verdict::Unknown));
        // In-band but noisy -> Warn, not Fail.
        let v = evaluate(Some(0.1), Some(100.0), Some(0.1), Some(2.0));
        assert_eq!(v[2].verdict, Verdict::Warn);
    }

    #[test]
    fn bad_args_rejected() {
        assert_eq!(cmd_overhead(&["--phase".to_string(), "5".to_string()]), 2);
        assert_eq!(cmd_overhead(&["--bogus".to_string()]), 2);
    }

    fn batt_point(
        t: f64,
        discharge_w: Option<f64>,
        rem_wh: Option<f64>,
    ) -> pf_core::session::BatteryPoint {
        let stamp = pf_core::telemetry::ClockStamp {
            wall_millis: (t * 1000.0) as u64,
            mono_millis: (t * 1000.0) as u64,
        };
        let tele = |v: Option<f64>| match v {
            Some(x) => pf_core::telemetry::Telemetry::measured(x, "battery", stamp),
            None => pf_core::telemetry::Telemetry::unavailable(
                pf_core::telemetry::UnavailKind::NotSampled,
                "test gap",
                "battery",
                stamp,
            ),
        };
        pf_core::session::BatteryPoint {
            t,
            discharge: tele(discharge_w),
            charge: tele(None),
            remaining_wh: tele(rem_wh),
            pct: tele(None),
        }
    }

    fn off_test_session(
        discharges: &[f64],
        rem_start_wh: Option<f64>,
        rem_end_wh: Option<f64>,
    ) -> SessionData {
        let mut s = SessionData {
            label: "test".to_string(),
            ..Default::default()
        };
        let n = discharges.len();
        for (idx, &w) in discharges.iter().enumerate() {
            let rem = match (rem_start_wh, rem_end_wh) {
                (Some(a), Some(b)) if n > 1 => Some(a + (b - a) * idx as f64 / (n - 1) as f64),
                (Some(a), _) => Some(a),
                _ => None,
            };
            s.battery.push(batt_point(idx as f64, Some(w), rem));
        }
        s
    }

    #[test]
    fn off_missing_planted_delta_fails() {
        // On-phases idle at ~5 W; the off gap drains 0.75 Wh over 10 min,
        // i.e. 4.5 W — a planted 0.5 W on-vs-off delta exceeds the 0.25 W band.
        let a = off_test_session(&[5.0; 60], Some(40.0), Some(39.92));
        let b = off_test_session(&[5.0; 60], Some(39.17), Some(39.08));
        let r = analyze_off_missing(&a, &b, 10.0);
        assert!((r.off_w.unwrap() - 4.5).abs() < 1e-9);
        assert!((r.delta_w.unwrap() - 0.5).abs() < 1e-9);
        assert_eq!(r.verdict, Verdict::Fail);
    }

    #[test]
    fn off_missing_no_endpoints_unknown() {
        let a = off_test_session(&[5.0; 60], None, None);
        let b = off_test_session(&[5.0; 60], None, None);
        let r = analyze_off_missing(&a, &b, 10.0);
        assert!(r.off_w.is_none());
        assert!(r.delta_w.is_none());
        assert_eq!(r.verdict, Verdict::Unknown);
        assert!(r.detail.contains("remaining_wh"));
    }

    #[test]
    fn self_cpu_delta_requires_both_phases() {
        let d = self_cpu_delta(Some(0.2), Some(0.7)).unwrap();
        assert!((d - 0.5).abs() < 1e-9);
        // A full-phase B median alone is not an incremental delta.
        assert_eq!(self_cpu_delta(None, Some(0.7)), None);
        assert_eq!(self_cpu_delta(Some(0.2), None), None);
        // Gating the verdict on the delta, not on the full-phase B cost: a
        // large B cost with a small delta passes.
        let v = evaluate(
            self_cpu_delta(Some(1.0), Some(1.1)),
            Some(100.0),
            Some(0.1),
            Some(0.2),
        );
        assert_eq!(v[0].verdict, Verdict::Pass);
        assert!(v[0].detail.contains("delta"), "{}", v[0].detail);
        // Without a phase-A baseline the CPU target is Unknown, not the B median.
        let v = evaluate(
            self_cpu_delta(None, Some(1.1)),
            Some(100.0),
            Some(0.1),
            Some(0.2),
        );
        assert_eq!(v[0].verdict, Verdict::Unknown);
    }

    #[test]
    fn phase_a_records_self_so_cpu_delta_is_measurable() {
        let p = overhead_phases();
        let a = p[0].2.as_deref().expect("phase A must select collectors");
        assert!(
            a.split(',').any(|c| c == "self"),
            "phase A must record self or the B-A delta is Unknown: {a}"
        );
        assert!(
            a.split(',').any(|c| c == "battery"),
            "phase A must keep battery for the discharge proxy: {a}"
        );
        // Phase B stays the full normal set (None => all collectors).
        assert!(p[1].2.is_none(), "phase B should remain the full set");
    }

    #[test]
    fn spread_is_dimensionless_in_details() {
        let v = evaluate(Some(0.1), Some(100.0), Some(0.1), Some(0.5));
        assert_eq!(v[2].verdict, Verdict::Pass);
        assert!(
            v[2].detail.contains("relative spread 0.50"),
            "{}",
            v[2].detail
        );
        assert!(!v[2].detail.contains("0.50 W"), "{}", v[2].detail);
        // Off-missing details use the same dimensionless label.
        let (verdict, detail) =
            evaluate_off_missing(Some(5.0), Some(5.0), Some(4.9), None, Some(0.3));
        assert_eq!(verdict, Verdict::Pass);
        assert!(detail.contains("relative spread 0.30"), "{detail}");
        assert!(!detail.contains("0.30 W"), "{detail}");
    }

    #[test]
    fn off_drain_rate_rejects_charging_gap() {
        // Remaining increased across the off window: charging, not drain.
        assert!(off_drain_rate(Some(39.0), Some(40.0), 0.1667).is_err());
        // Valid discharge is accepted.
        assert!((off_drain_rate(Some(40.0), Some(39.0), 0.5).unwrap() - 2.0).abs() < 1e-9);
        // Missing endpoints remain an error with the documented reason.
        assert!(off_drain_rate(None, Some(40.0), 0.5).is_err());
    }

    #[test]
    fn off_missing_charging_gap_is_unknown_with_reason() {
        // A ends at 39.0 Wh, B starts at 39.5 Wh: the battery charged during
        // the off gap, so the negative "drain" must not inflate the delta.
        let a = off_test_session(&[5.0; 60], Some(39.0), Some(39.0));
        let b = off_test_session(&[5.0; 60], Some(39.5), Some(39.5));
        let r = analyze_off_missing(&a, &b, 10.0);
        assert!(r.off_w.is_none());
        assert!(r.off_w_reason.as_deref().unwrap().contains("charging"));
        assert!(r.delta_w.is_none());
        assert_eq!(r.verdict, Verdict::Unknown);
        assert!(r.detail.contains("charging"), "{}", r.detail);
    }

    #[test]
    fn off_missing_bad_args_rejected() {
        assert_eq!(cmd_overhead(&["--off-missing".to_string()]), 2);
        assert_eq!(
            cmd_overhead(&["--off-missing".to_string(), "a.jsonl".to_string()]),
            2
        );
        assert_eq!(
            cmd_overhead(&[
                "--off-missing".to_string(),
                "a.jsonl".to_string(),
                "b.jsonl".to_string(),
                "c.jsonl".to_string()
            ]),
            2
        );
        assert_eq!(
            cmd_overhead(&[
                "--off-missing".to_string(),
                "a.jsonl".to_string(),
                "b.jsonl".to_string(),
                "--off-minutes".to_string(),
                "0".to_string()
            ]),
            2
        );
        assert_eq!(
            cmd_overhead(&[
                "--off-missing".to_string(),
                "a.jsonl".to_string(),
                "b.jsonl".to_string(),
                "--off-minutes".to_string()
            ]),
            2
        );
        assert_eq!(
            cmd_overhead(&[
                "--off-missing".to_string(),
                "a.jsonl".to_string(),
                "b.jsonl".to_string(),
                "--phase".to_string(),
                "60".to_string()
            ]),
            2
        );
    }
}
