//! power-forensics: Windows laptop power profiler and battery-forensics tool.
//!
//! Foundation (MVP phase 1-2): battery + OS power collectors, dual-clock
//! timestamps, provenance-tagged readings, JSONL sessions, rule-based
//! analysis helpers. No faked sensors: unavailable stays unavailable.

mod overhead;

use std::io::Write;

use pf_agent::ipc_server;
use pf_agent::monitor::{self, MonitorOpts, capabilities_json, cmd_sample};
use pf_agent::service;
use pf_agent::store;
use pf_core::{analysis, archive, calib, dashboard, ipc, json, session, stats, timeline};

fn usage() -> &'static str {
    "usage:\n  \
     power-forensics capabilities [--human]\n  \
     power-forensics sample\n  \
     power-forensics monitor [--interval-ms N] [--samples N] [--out PATH] [--note TEXT]\n  \
                    [--helper PF-ELEVATED --helper-every SEC] [--collectors a,b,c] [--preset normal|low|deep]\n  \
     power-forensics agent [--out PATH] [--interval-ms N] [--samples N] [--note TEXT]\n  \
                    [--helper PF-ELEVATED --helper-every SEC] [--collectors a,b,c] [--preset normal|low|deep]\n  \
                    (persistent: serves the named pipe and owns monitoring; authoritative stop is\n  \
                     `service stop --pipe` / `ipc send stop --pipe`; <out-dir>/stop.requested also works)\n  \
     power-forensics diagnose FILE [--baseline BASE.json] [--save-baseline OUT.json] [--redact]\n  \
     power-forensics compare A.jsonl B.jsonl [--name TEXT] [--redact]\n  \
     power-forensics report FILE [--redact]\n  \
     power-forensics sessions [DIR] [--prune-days N --yes] [--max-mb N --yes]\n  \
     power-forensics export FILE --out DATA.csv [--redact] [--force]\n  \
     power-forensics dashboard FILE [--redact]\n  \
     power-forensics timeline FILE [--width N] [--redact]\n  \
     power-forensics freshness FILE\n  \
      power-forensics ipc (serve [DIR]|status [DIR]|send CMD [ARG] [--dir DIR] [--pipe])\n  \
      power-forensics service (status|stop|pause|resume|marker) [--pipe]\n  \
     power-forensics recover FILE\n  \
     power-forensics experiment --name N --label-a A --label-b B --phase-a SEC --phase-b SEC [--interval-ms N] [--reps 1..8] [--settle SEC] [--redact] [--yes]\n  \
      power-forensics overhead [--phase SEC] [--interval-ms N]\n  \
      power-forensics overhead --off-missing A.jsonl B.jsonl [--off-minutes N]\n  \
     power-forensics calibrate [--levels 0,20,40,60,80,100] [--seconds N] [--machine NAME] [--panel NAME] [--yes]\n  \
     power-forensics store FILE [--out DIR] [--estimate] [--force]\n  \
     \n  \
     --samples N stops the session after N samples of the primary collector\n  \
     (battery when active, otherwise the first --collectors entry), so it bounds\n  \
     every collector subset instead of silently doing nothing. It is a SAMPLE\n  \
     bound, not a wall-time bound: a paused session records no samples, so the\n  \
     bound does not advance while paused. A collector that produces no sample\n  \
     at all for 30s lets the next producing collector take over the bound."
}

fn load_session_file(path: &str) -> Result<session::SessionData, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let label = std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    session::load_session(&label, &text)
}

/// Sensitive tokens fed to `analysis::redact_report_identifiers`: process
/// names (as `process-N`), and the bare hardware/network/identity values that
/// appear as ordinary rendered values (GPU/display/network/USB/battery
/// names, Wi-Fi SSIDs, session label, current host/user). Deriving the set
/// from the session model lets the redactor mask bare values that carry no
/// `key:`/`key=` syntax, without broad substring replacement.
fn session_identifiers(sessions: &[&session::SessionData]) -> Vec<analysis::RedactToken> {
    // Shared with the GUI/export path so redaction cannot drift between
    // interfaces; the implementation lives in pf-core.
    pf_core::export::session_identifiers(sessions)
}

/// Apply redaction to already-rendered text when `--redact` is set;
/// otherwise return it unchanged (redaction stays strictly opt-in).
fn redact_output(text: &str, redact: bool, sessions: &[&session::SessionData]) -> String {
    if redact {
        analysis::redact_report_identifiers(text, &session_identifiers(sessions))
    } else {
        text.to_string()
    }
}

/// Current machine + primary-panel identity used to gate calibration reuse.
/// The panel comes from the display collector's primary monitor id (the
/// EDID-style hardware token, e.g. AUOA5AB); "unknown" when no display can be
/// enumerated. This is what `calibrate` persists and `report` re-checks.
fn current_display_identity() -> (String, String) {
    let machine = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string());
    let panel = pf_collectors::display::DisplayCollector::new()
        .read()
        .ok()
        .and_then(|s| {
            let d = s
                .displays
                .iter()
                .find(|d| d.primary)
                .or_else(|| s.displays.first())?;
            let token = pf_collectors::display::monitor_token(&d.name);
            // Prefer the EDID-style monitor token; fall back to whatever the
            // collector named the display (e.g. an adapter path) if degenerate.
            Some(if token.is_empty() || token == "." {
                d.name.clone()
            } else {
                token
            })
        })
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    (machine, panel)
}

/// Load the persisted display calibration, if one exists AND belongs to this
/// machine/panel. A calibration from another machine must never turn this
/// machine's brightness into watts, so the identity is always checked.
fn load_display_model() -> Option<calib::DisplayModel> {
    let (machine, panel) = current_display_identity();
    calib::load_display_model_for(&machine, &panel)
}

fn cmd_diagnose(args: &[String]) -> i32 {
    let mut file: Option<String> = None;
    let mut baseline_path: Option<String> = None;
    let mut save_path: Option<String> = None;
    let mut redact = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--baseline" => {
                i += 1;
                baseline_path = args.get(i).cloned();
            }
            "--save-baseline" => {
                i += 1;
                save_path = args.get(i).cloned();
            }
            "--redact" => redact = true,
            other if !other.starts_with("--") && file.is_none() => {
                file = Some(other.to_string());
            }
            other => {
                eprintln!("error: bad diagnose arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let file = match file {
        Some(f) => f,
        None => {
            eprintln!("error: diagnose needs a session FILE");
            return 2;
        }
    };
    let s = match load_session_file(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let baseline_median = baseline_path
        .map(|p| {
            std::fs::read_to_string(&p)
                .map_err(|e| format!("cannot read baseline {p}: {e}"))
                .and_then(|t| json::parse(&t).map_err(|e| format!("bad baseline JSON: {e}")))
                .and_then(|v| {
                    analysis::baseline_from_json(&v)
                        .ok_or("baseline JSON lacks required fields".to_string())
                })
        })
        .transpose();
    let baseline_median = match baseline_median {
        Ok(opt) => opt.map(|b| b.median),
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    if let Some(out) = save_path {
        match analysis::baseline_of(&s.label, &s) {
            Some(b) => {
                if let Err(e) = std::fs::write(&out, analysis::baseline_to_json(&b)) {
                    eprintln!("error: cannot write baseline: {e}");
                    return 2;
                }
                println!("baseline saved to {out}");
            }
            None => {
                eprintln!("error: no discharge data to baseline");
                return 2;
            }
        }
    }
    let d = analysis::diagnose_session(&s, baseline_median);
    let text = analysis::render_diagnosis(&s.label, &d);
    print!("{}", redact_output(&text, redact, &[&s]));
    0
}

fn cmd_compare(args: &[String]) -> i32 {
    let mut files: Vec<String> = Vec::new();
    let mut name: Option<String> = None;
    let mut redact = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                i += 1;
                name = args.get(i).cloned();
            }
            "--redact" => redact = true,
            other if !other.starts_with("--") => files.push(other.to_string()),
            other => {
                eprintln!("error: bad compare arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    if files.len() != 2 {
        eprintln!("error: compare needs exactly two session files (A B)");
        return 2;
    }
    let mut labels = Vec::new();
    let mut sessions = Vec::new();
    for f in &files {
        match load_session_file(f) {
            Ok(mut s) => {
                if labels.len() == 1 {
                    if let Some(n) = &name {
                        s.label = format!("B ({n})");
                    }
                } else if labels.is_empty() {
                    // keep filename label for A
                }
                labels.push(s.label.clone());
                sessions.push(s);
            }
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        }
    }
    let c = analysis::compare_sessions(&sessions[0], &sessions[1]);
    let text = analysis::render_comparison(&c);
    print!(
        "{}",
        redact_output(&text, redact, &[&sessions[0], &sessions[1]])
    );
    0
}

fn cmd_report(args: &[String]) -> i32 {
    let mut redact = false;
    let mut file: Option<&String> = None;
    for a in args {
        match a.as_str() {
            "--redact" => redact = true,
            other if !other.starts_with("--") && file.is_none() => file = Some(a),
            other => {
                eprintln!("error: bad report arg: {other}");
                return 2;
            }
        }
    }
    let Some(file) = file else {
        eprintln!("error: report needs exactly one session FILE [--redact]");
        return 2;
    };
    let s = match load_session_file(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let d = analysis::diagnose_session(&s, None);
    let mut text = analysis::render_report(&s, &d);
    // A matching calibration turns brightness into a derived display-power
    // estimate; without one, display power stays unavailable in the report.
    if let Some(model) = load_display_model()
        && let Some(b) = stats::median(
            &s.display
                .iter()
                .filter_map(|p| p.brightness.val())
                .collect::<Vec<_>>(),
        )
        && let Some(w) = model.display_w_at(b)
    {
        text.push_str(&format!(
            "Display power [derived from calibration]: at {b:.0}% brightness ~ {w:.2} W above {:.0}% (R^2 {:.2}, panel {}); model is a system-level delta, not a display sensor\n",
            model.min_brightness, model.r2, model.panel
        ));
    }
    print!("{}", redact_output(&text, redact, &[&s]));
    0
}

/// Sorted `.jsonl` files in DIR. Falls back to the current directory when DIR
/// cannot be listed; used both for the listing and for re-reading the current
/// disk state between retention policies.
fn list_jsonl(dir: &str) -> std::io::Result<Vec<std::path::PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => std::fs::read_dir(".")?,
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort();
    Ok(files)
}

/// Browse recorded sessions: one line per file from header + footer only
/// (fast even for long sessions; no full parse). Also flags sessions with no
/// footer (the process was killed or crashed) and can prune old recordings.
fn cmd_sessions(args: &[String]) -> i32 {
    let mut dir = "sessions".to_string();
    let mut prune_days: Option<u64> = None;
    let mut max_mb: Option<u64> = None;
    let mut yes = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--prune-days" => {
                i += 1;
                prune_days = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &u64| *v > 0);
                if prune_days.is_none() {
                    eprintln!("error: invalid --prune-days (positive days)");
                    return 2;
                }
            }
            "--max-mb" => {
                i += 1;
                max_mb = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &u64| *v > 0);
                if max_mb.is_none() {
                    eprintln!("error: invalid --max-mb (positive megabytes)");
                    return 2;
                }
            }
            "--yes" => yes = true,
            other if !other.starts_with("--") && dir == "sessions" => dir = other.to_string(),
            other => {
                eprintln!("error: bad sessions arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let mut files = match list_jsonl(&dir) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("error: cannot list {dir}: {err}");
            return 2;
        }
    };
    if files.is_empty() {
        println!("no .jsonl sessions in {dir}");
        return 0;
    }
    println!("FILE  STATUS  SAMPLES  DUR_S  DISCH_MED_W  DISCH_WH  CHG_WH  CPU_MED_%  NOTE");
    for path in &files {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut header_wall = 0u64;
                let mut note = String::new();
                let mut footer: Option<json::JVal> = None;
                let mut footer_wall = 0u64;
                let mut has_header = false;
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(v) = json::parse(line) else { continue };
                    if v.get("type").and_then(|t| t.as_str()) == Some("session_header") {
                        has_header = true;
                        header_wall = v.get("wall_ms").and_then(|m| m.num()).unwrap_or(0.0) as u64;
                        note = v
                            .get("note")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                    } else if v.get("type").and_then(|t| t.as_str()) == Some("session_footer") {
                        footer_wall = v.get("wall_ms").and_then(|m| m.num()).unwrap_or(0.0) as u64;
                        footer = v.get("summary").cloned();
                    }
                }
                // A footer is only written on a clean stop; its absence means
                // the session ended unexpectedly (crash/kill/power loss).
                let status = if footer.is_some() {
                    "ok"
                } else if has_header {
                    "incomplete"
                } else {
                    "no-header"
                };
                let num = |k: &str| {
                    footer
                        .as_ref()
                        .and_then(|f| f.get(k))
                        .and_then(|v| v.num())
                        .map(|x| format!("{x:.2}"))
                        .unwrap_or_else(|| "-".to_string())
                };
                let int = |k: &str| {
                    footer
                        .as_ref()
                        .and_then(|f| f.get(k))
                        .and_then(|v| v.num())
                        .map(|x| format!("{:.0}", x))
                        .unwrap_or_else(|| "-".to_string())
                };
                let dur = if footer_wall > header_wall {
                    format!("{}", (footer_wall - header_wall) / 1000)
                } else {
                    "-".to_string()
                };
                println!(
                    "{}  {}  {}  {}  {}  {}  {}  {}  {}",
                    name,
                    status,
                    int("samples"),
                    dur,
                    num("discharge_median_w"),
                    num("discharge_wh"),
                    num("charge_wh"),
                    num("cpu_utility_median_pct"),
                    note.chars().take(40).collect::<String>(),
                );
            }
            Err(e) => eprintln!("error: cannot read {}: {e}", path.display()),
        }
    }
    if let Some(days) = prune_days {
        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        let mut removed = 0usize;
        for path in &files {
            let old = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map(|m| m < cutoff)
                .unwrap_or(false);
            if !old {
                continue;
            }
            if yes {
                match std::fs::remove_file(path) {
                    Ok(()) => {
                        removed += 1;
                        println!("pruned {}", path.display());
                    }
                    Err(e) => eprintln!("error: cannot remove {}: {e}", path.display()),
                }
            } else {
                println!("would prune {} (rerun with --yes)", path.display());
            }
        }
        println!(
            "retention: {removed} file(s) removed older than {days} day(s){}",
            if yes { "" } else { " (dry run)" }
        );
    }
    if let Some(mb) = max_mb {
        // A previous policy (--prune-days) may have just deleted files, so
        // re-list: the budget decision must see the current disk, never a
        // stale pre-delete snapshot.
        files = match list_jsonl(&dir) {
            Ok(f) => f,
            Err(err) => {
                eprintln!("error: cannot list {dir}: {err}");
                return 2;
            }
        };
        let budget = mb * 1024 * 1024;
        let mut infos: Vec<(String, std::path::PathBuf, u64, Option<u64>)> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        for p in &files {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            match std::fs::metadata(p) {
                Ok(m) => {
                    let mtime = m
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs());
                    if mtime.is_none() {
                        unknown.push(name.clone());
                    }
                    infos.push((name, p.clone(), m.len(), mtime));
                }
                // Unreadable metadata is never treated as "oldest": it is
                // classified unknown and kept out of the prune plan.
                Err(e) => unknown.push(format!("{name} ({e})")),
            }
        }
        for n in &unknown {
            eprintln!("warning: skipping {n}: age unreadable, not treated as oldest");
        }
        let total: u64 = infos.iter().filter_map(|(_, _, b, m)| m.map(|_| *b)).sum();
        if total <= budget {
            println!(
                "storage: {:.2} MB in {} file(s), under {} MB budget",
                total as f64 / 1_048_576.0,
                infos.len(),
                mb
            );
        } else {
            let entries: Vec<(String, u64, Option<u64>)> = infos
                .iter()
                .map(|(n, _, b, m)| (n.clone(), *b, *m))
                .collect();
            let victims = store::plan_budget_prune(&entries, budget);
            let mut removed = 0usize;
            for name in &victims {
                if let Some((_, path, _, _)) = infos.iter().find(|(n, _, _, _)| n == name) {
                    if yes {
                        match std::fs::remove_file(path) {
                            Ok(()) => {
                                removed += 1;
                                println!("pruned {}", path.display());
                            }
                            Err(e) => eprintln!("error: cannot remove {}: {e}", path.display()),
                        }
                    } else {
                        println!("would prune {} (rerun with --yes)", path.display());
                    }
                }
            }
            println!(
                "storage: {} file(s) over {} MB budget{}",
                victims.len(),
                mb,
                if yes {
                    format!(" ({removed} removed)")
                } else {
                    " (dry run)".to_string()
                }
            );
        }
    }
    0
}

/// Guided display-power calibration. The human sets each brightness level and
/// presses Enter; the tool records an idle window, fits power vs brightness,
/// and persists the model only when the fit is good enough. Runs unplugged.
fn cmd_calibrate(args: &[String]) -> i32 {
    let mut levels_arg = "0,20,40,60,80,100".to_string();
    let mut seconds = 30u64;
    // Persist the real machine/panel identity so the reuse gate is not inert.
    let (default_machine, default_panel) = current_display_identity();
    let mut machine = default_machine;
    let mut panel = default_panel;
    let mut yes = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--levels" => {
                i += 1;
                levels_arg = args.get(i).cloned().unwrap_or(levels_arg);
            }
            "--seconds" => {
                i += 1;
                seconds = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &u64| (10..=600).contains(v))
                    .unwrap_or(0);
                if seconds == 0 {
                    eprintln!("error: invalid --seconds (10..600)");
                    return 2;
                }
            }
            "--machine" => {
                i += 1;
                machine = args.get(i).cloned().unwrap_or(machine);
            }
            "--panel" => {
                i += 1;
                panel = args.get(i).cloned().unwrap_or(panel);
            }
            "--yes" => yes = true,
            other => {
                eprintln!("error: bad calibrate arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let levels: Vec<f64> = match levels_arg
        .split(',')
        .map(|s| s.trim().parse::<f64>())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) if (2..=12).contains(&v.len()) && v.iter().all(|x| (0.0..=100.0).contains(x)) => v,
        _ => {
            eprintln!("error: --levels needs 2..12 values in 0..100, comma-separated");
            return 2;
        }
    };
    if std::fs::create_dir_all("sessions").is_err() {
        eprintln!("error: cannot create sessions dir");
        return 2;
    }
    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut points: Vec<(f64, f64)> = Vec::new();
    for level in &levels {
        if !yes {
            println!("--- set display brightness to {level:.0}% ---");
            println!(
                "Keep the machine idle and unplugged, then press Enter to record {seconds}s..."
            );
            let mut buf = String::new();
            if std::io::stdin().read_line(&mut buf).is_err() {
                eprintln!("error: stdin unreadable (use --yes for non-interactive)");
                return 2;
            }
        } else {
            println!("--- recording at {level:.0}% for {seconds}s ---");
        }
        let out = format!("sessions/calib-{level:.0}-{run_id}.jsonl");
        let code = monitor::cmd_monitor(MonitorOpts {
            interval_ms: 1000,
            samples: Some(seconds),
            out: Some(out.clone()),
            note: format!("display calibration at {level:.0}%"),
            helper: None,
            helper_every: 60,
            collectors: None,
            preset: None,
        });
        if code != 0 {
            eprintln!("error: calibration recording failed");
            return 2;
        }
        let s = match load_session_file(&out) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {e}");
                return 2;
            }
        };
        let idle = analysis::idle_subset(&s);
        let median = stats::median(&idle).or_else(|| {
            stats::median(
                &s.battery
                    .iter()
                    .filter_map(|p| p.discharge.val())
                    .collect::<Vec<_>>(),
            )
        });
        match median {
            Some(m) => {
                println!("  {level:.0}%: idle median {m:.3} W");
                points.push((*level, m));
            }
            None => {
                eprintln!(
                    "  {level:.0}%: no discharge data — run calibration unplugged with the screen on"
                );
                return 2;
            }
        }
    }
    let fit = match calib::fit_linear(&points) {
        Some(f) => f,
        None => {
            eprintln!("error: not enough distinct levels to fit a model");
            return 2;
        }
    };
    let min_brightness = points.iter().map(|(b, _)| *b).fold(f64::INFINITY, f64::min);
    let model = calib::DisplayModel {
        machine,
        panel,
        min_brightness,
        slope_w_per_pct: fit.slope,
        intercept_w: fit.intercept,
        r2: fit.r2,
        created_ms: run_id,
        levels: points.clone(),
    };
    let path = "sessions/display-calibration.json";
    if let Err(e) = std::fs::write(path, model.to_json()) {
        eprintln!("error: cannot write {path}: {e}");
        return 2;
    }
    println!(
        "\ndisplay model: {:.4} W/% above {:.0}% brightness, intercept {:.2} W, R² {:.2} (n={})",
        model.slope_w_per_pct, min_brightness, model.intercept_w, model.r2, fit.n
    );
    if model.r2 < calib::MIN_R2 {
        println!(
            "[warn] fit below R² {:.1}: display power stays [unavailable] until a cleaner sweep",
            calib::MIN_R2
        );
    }
    println!("wrote {path}");
    0
}

/// Export a session to long-format CSV for manual analysis
/// (spreadsheet pivot on collector/metric).
///
/// Writes rows incrementally to a `BufWriter` so the full CSV string is never
/// materialized, and refuses to overwrite an existing file unless `--force` is
/// given (create_new semantics, matching the monitor/store writers).
fn cmd_export(args: &[String]) -> i32 {
    let mut file: Option<String> = None;
    let mut out: Option<String> = None;
    let mut redact = false;
    let mut force = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = args.get(i).cloned();
            }
            "--redact" => redact = true,
            "--force" => force = true,
            other if !other.starts_with("--") && file.is_none() => {
                file = Some(other.to_string());
            }
            other => {
                eprintln!("error: bad export arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let (Some(file), Some(out)) = (file, out) else {
        eprintln!("error: export needs FILE and --out DATA.csv [--redact] [--force]");
        return 2;
    };
    let s = match load_session_file(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let names = session_identifiers(&[&s]);
    let handle = if force {
        std::fs::File::create(&out)
    } else {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&out)
    };
    let handle = match handle {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!("error: refusing to overwrite existing {out} (use --force)");
            return 1;
        }
        Err(e) => {
            eprintln!("error: cannot write {out}: {e}");
            return 2;
        }
    };
    let mut w = std::io::BufWriter::new(handle);
    let written = (|| -> std::io::Result<usize> {
        w.write_all(session::CSV_HEADER.as_bytes())?;
        w.write_all(b"\n")?;
        let mut rows = 0usize;
        for row in session::csv_rows_iter(&s) {
            let line = session::format_csv_row(&row);
            if redact {
                w.write_all(analysis::redact_report_identifiers(&line, &names).as_bytes())?;
            } else {
                w.write_all(line.as_bytes())?;
            }
            rows += 1;
        }
        w.flush()?;
        Ok(rows)
    })();
    match written {
        Ok(rows) => {
            println!(
                "exported {rows} rows to {out} (long format: pivot on collector/metric; provenance column included)"
            );
            0
        }
        Err(e) => {
            eprintln!("error: cannot write {out}: {e}");
            2
        }
    }
}

/// Single-repetition A/B analysis dispatch used by the CLI: routes through the
/// settle-aware comparison so `--settle` trims the manual condition-change
/// transient instead of being parsed and ignored.
fn analyze_single_experiment(
    a: &session::SessionData,
    b: &session::SessionData,
    settle_secs: u64,
) -> analysis::Comparison {
    analysis::compare_single_with_settle(a, b, settle_secs as f64)
}

/// One recorded repetition of one condition after settling was removed:
/// the block median, its capture quality, the block's CPU median (background
/// mismatch), and the trimmed session (capacity/runtime projection).
struct ExpBlock {
    median: Option<f64>,
    quality: analysis::CaptureQuality,
    cpu_median: Option<f64>,
    session: session::SessionData,
}

/// Multi-rep experiment analysis: aggregate the balanced blocks and build the
/// full `Comparison` via the same runtime/validity path as the single-rep
/// comparison, with per-block background CPU mismatch wired into confidence.
fn analyze_repeated_experiment(
    label_a: &str,
    label_b: &str,
    a_blocks: &[ExpBlock],
    b_blocks: &[ExpBlock],
) -> Option<analysis::Comparison> {
    let meds = |bs: &[ExpBlock]| bs.iter().filter_map(|b| b.median).collect::<Vec<_>>();
    let ma = stats::median(&meds(a_blocks));
    let mb = stats::median(&meds(b_blocks));
    let delta = match (ma, mb) {
        (Some(x), Some(y)) => Some(y - x),
        _ => None,
    };
    let a_cpu: Vec<f64> = a_blocks.iter().filter_map(|b| b.cpu_median).collect();
    let b_cpu: Vec<f64> = b_blocks.iter().filter_map(|b| b.cpu_median).collect();
    let qa = analysis::aggregate_quality(&a_blocks.iter().map(|b| b.quality).collect::<Vec<_>>());
    let qb = analysis::aggregate_quality(&b_blocks.iter().map(|b| b.quality).collect::<Vec<_>>());
    let (confidence, confidence_note, confounders) =
        analysis::experiment_confidence(&qa, &qb, delta, &a_cpu, &b_cpu);
    let mut a_sess = a_blocks.first()?.session.clone();
    let mut b_sess = b_blocks.first()?.session.clone();
    a_sess.label = label_a.to_string();
    b_sess.label = label_b.to_string();
    Some(analysis::comparison_from_stats(
        &a_sess,
        &b_sess,
        ma,
        mb,
        &qa,
        &qb,
        confidence,
        confidence_note,
        confounders,
        Vec::new(),
    ))
}

/// Guided A/B experiment: the human flips the condition (Bluetooth,
/// brightness, power plan...), the tool records both phases and compares.
/// No automated OS toggles — those need privileges and are fragile; the
/// measurement and analysis are what we guarantee.
fn cmd_experiment(args: &[String]) -> i32 {
    let mut name: Option<String> = None;
    let mut label_a = "condition A".to_string();
    let mut label_b = "condition B".to_string();
    let mut phase_a = 0u64;
    let mut phase_b = 0u64;
    let mut interval_ms = 1000u64;
    let mut yes = false;
    let mut reps = 1u64;
    let mut settle_secs = 0u64;
    let mut redact = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                i += 1;
                name = args.get(i).cloned();
            }
            "--redact" => redact = true,
            "--label-a" => {
                i += 1;
                label_a = args.get(i).cloned().unwrap_or(label_a);
            }
            "--label-b" => {
                i += 1;
                label_b = args.get(i).cloned().unwrap_or(label_b);
            }
            "--phase-a" => {
                i += 1;
                phase_a = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            "--phase-b" => {
                i += 1;
                phase_b = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            "--interval-ms" => {
                i += 1;
                interval_ms = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1000)
                    .clamp(100, 60_000);
            }
            "--yes" => yes = true,
            "--reps" => {
                i += 1;
                reps = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v| (1..=8).contains(v))
                    .unwrap_or(0);
                if reps == 0 {
                    eprintln!("error: invalid --reps (1..8)");
                    return 2;
                }
            }
            "--settle" => {
                i += 1;
                settle_secs = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v| *v <= 300)
                    .unwrap_or(u64::MAX);
                if settle_secs == u64::MAX {
                    eprintln!("error: invalid --settle (0..300 seconds)");
                    return 2;
                }
            }
            other => {
                eprintln!("error: bad experiment arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let (Some(name), true, true) = (name, phase_a >= 5, phase_b >= 5) else {
        eprintln!("error: experiment needs --name and --phase-a/--phase-b >= 5 (seconds)");
        return 2;
    };
    if let Err(e) = std::fs::create_dir_all("sessions") {
        eprintln!("error: cannot create sessions dir: {e}");
        return 2;
    }
    let run_recording = |out: &str, note: &str, label: &str, secs: u64| -> bool {
        if !yes {
            println!("--- {label} ---");
            println!("Set up the condition now, then press Enter to record {secs}s...");
            let mut buf = String::new();
            if std::io::stdin().read_line(&mut buf).is_err() {
                eprintln!("error: stdin unreadable (use --yes for non-interactive)");
                return false;
            }
        } else {
            println!("--- {label} ({secs}s) ---");
        }
        let samples = (secs * 1000 / interval_ms).max(1);
        let code = monitor::cmd_monitor(MonitorOpts {
            interval_ms,
            samples: Some(samples),
            out: Some(out.to_string()),
            note: note.to_string(),
            helper: None,
            helper_every: 60,
            collectors: None,
            preset: None,
        });
        if code != 0 {
            eprintln!("error: recording failed: {out}");
            return false;
        }
        true
    };

    if reps == 1 {
        let a = format!("sessions/exp-{name}-a.jsonl");
        let b = format!("sessions/exp-{name}-b.jsonl");
        if !run_recording(
            &a,
            &format!("experiment {name} phase a: {label_a}"),
            &format!("phase a: {label_a}"),
            phase_a,
        ) {
            return 2;
        }
        if !run_recording(
            &b,
            &format!("experiment {name} phase b: {label_b}"),
            &format!("phase b: {label_b}"),
            phase_b,
        ) {
            return 2;
        }
        let (sa, sb) = match (load_session_file(&a), load_session_file(&b)) {
            (Ok(x), Ok(y)) => (x, y),
            _ => {
                eprintln!("error: could not reload recorded phases");
                return 2;
            }
        };
        let mut sb_named = sb;
        sb_named.label = format!("B ({label_b})");
        let c = analyze_single_experiment(&sa, &sb_named, settle_secs);
        let mut report = format!("\nEXPERIMENT {name}: {label_a} vs {label_b}\n");
        report.push_str(&analysis::render_comparison(&c));
        print!("{}", redact_output(&report, redact, &[&sa, &sb_named]));
        return 0;
    }

    // Repeated balanced blocks (A B B A ...): each block is recorded fresh and
    // the first `settle_secs` are discarded, so transients and order effects
    // cannot masquerade as a condition effect.
    let mut a_blocks: Vec<ExpBlock> = Vec::new();
    let mut b_blocks: Vec<ExpBlock> = Vec::new();
    let mut report = String::new();
    for r in 0..reps {
        let forward = r % 2 == 0;
        let order: [(&str, &str, u64); 2] = if forward {
            [("a", &label_a, phase_a), ("b", &label_b, phase_b)]
        } else {
            [("b", &label_b, phase_b), ("a", &label_a, phase_a)]
        };
        for (tag, label, secs) in order {
            let out = format!("sessions/exp-{name}-r{r}-{tag}.jsonl");
            let note = format!("experiment {name} rep {} {tag}: {label}", r + 1);
            if !run_recording(
                &out,
                &note,
                &format!("rep {} phase {tag}: {label}", r + 1),
                secs,
            ) {
                return 2;
            }
            let s = match load_session_file(&out) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    return 2;
                }
            };
            let trimmed = analysis::trim_settle(&s, settle_secs as f64);
            let (med, q) = analysis::block_median(&trimmed);
            let cpu_median = analysis::block_cpu_median(&trimmed);
            report.push_str(&format!(
                "  rep {} {tag}: median {} ({})\n",
                r + 1,
                med.map(|m| format!("{m:.2} W"))
                    .unwrap_or_else(|| "n/a".to_string()),
                q.coverage
                    .map(|c| format!("coverage {:.0}%", c * 100.0))
                    .unwrap_or_else(|| "coverage n/a".to_string())
            ));
            let entry = ExpBlock {
                median: med,
                quality: q,
                cpu_median,
                session: trimmed,
            };
            if tag == "a" {
                a_blocks.push(entry);
            } else {
                b_blocks.push(entry);
            }
        }
    }
    report.push_str(&format!(
        "\nEXPERIMENT {name}: {label_a} vs {label_b} ({reps} balanced repetitions, {settle_secs}s settling discarded)\n"
    ));
    match analyze_repeated_experiment(&label_a, &label_b, &a_blocks, &b_blocks) {
        Some(c) => {
            report.push_str(&analysis::render_comparison(&c));
            let sessions: Vec<&session::SessionData> = a_blocks
                .iter()
                .chain(b_blocks.iter())
                .map(|b| &b.session)
                .collect();
            print!("{}", redact_output(&report, redact, &sessions));
            0
        }
        None => {
            eprintln!("error: experiment recorded no usable blocks");
            2
        }
    }
}

/// Migrate a JSONL session into the compact columnar archive.
fn cmd_store(args: &[String]) -> i32 {
    let mut file: Option<String> = None;
    let mut out: Option<String> = None;
    let mut estimate = false;
    let mut force = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = args.get(i).cloned();
            }
            "--estimate" => estimate = true,
            "--force" => force = true,
            other if !other.starts_with("--") && file.is_none() => {
                file = Some(other.to_string());
            }
            other => {
                eprintln!("error: bad store arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("error: store needs FILE [--out DIR] [--estimate] [--force]");
        return 2;
    };
    if estimate {
        let bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        match store::session_duration_s(&text) {
            Some(dur) => match store::estimate_mb_per_hour(bytes, dur) {
                Some(mb_h) => {
                    println!("estimate: {mb_h:.2} MB/hour ({bytes} bytes over {dur:.0}s)")
                }
                None => {
                    eprintln!("error: cannot estimate rate for {file}");
                    return 1;
                }
            },
            None => {
                eprintln!(
                    "error: cannot estimate {file}: no header/footer duration (incomplete session?)"
                );
                return 1;
            }
        }
        if out.is_none() {
            return 0;
        }
    }
    let Some(out) = out else {
        eprintln!("error: store needs FILE and --out DIR");
        return 2;
    };
    let s = match load_session_file(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let arch = archive::migrate(&s.label, &file, &s);
    let dir = std::path::Path::new(&out);
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("error: cannot create {out}: {e}");
        return 2;
    }
    // Explicit overwrite semantics, matching `export`: refuse to clobber an
    // existing archive unless --force. When forced, write temp siblings first
    // and rename over the targets, so a failed write never truncates the
    // existing archive before the replacement is complete.
    let targets: [(&str, Vec<u8>); 3] = [
        ("manifest.json", arch.manifest.into_bytes()),
        ("samples.bin", arch.samples),
        ("events.jsonl", arch.events.join("\n").into_bytes()),
    ];
    let existing: Vec<&str> = targets
        .iter()
        .filter(|(name, _)| dir.join(name).exists())
        .map(|(name, _)| *name)
        .collect();
    if !existing.is_empty() && !force {
        eprintln!(
            "error: refusing to overwrite existing archive in {out} ({}); use --force",
            existing.join(", ")
        );
        return 1;
    }
    let mut temps: Vec<std::path::PathBuf> = Vec::new();
    let cleanup = |temps: &[std::path::PathBuf]| {
        for t in temps {
            let _ = std::fs::remove_file(t);
        }
    };
    for (name, bytes) in &targets {
        let tmp = dir.join(format!("{name}.tmp{}", std::process::id()));
        if let Err(e) = std::fs::write(&tmp, bytes) {
            let _ = std::fs::remove_file(&tmp);
            cleanup(&temps);
            eprintln!("error: cannot write archive: {e}");
            return 2;
        }
        temps.push(tmp);
    }
    for ((name, _), tmp) in targets.iter().zip(&temps) {
        if let Err(e) = std::fs::rename(tmp, dir.join(name)) {
            cleanup(&temps);
            eprintln!("error: cannot replace {name}: {e}");
            return 2;
        }
    }
    let jsonl_bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    let arch_bytes: u64 = targets.iter().map(|(_, b)| b.len() as u64).sum();
    let ratio = if arch_bytes > 0 {
        jsonl_bytes as f64 / arch_bytes as f64
    } else {
        0.0
    };
    println!(
        "archived {} records to {out} ({jsonl_bytes} -> {arch_bytes} bytes, {ratio:.1}x smaller)",
        arch.records
    );
    0
}

fn now_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Text dashboard: SYSTEM POWER / BATTERY / RUNTIME cards + provenance.
fn cmd_dashboard(args: &[String]) -> i32 {
    let mut file: Option<&String> = None;
    let mut redact = false;
    for a in args {
        match a.as_str() {
            "--redact" => redact = true,
            other if !other.starts_with("--") && file.is_none() => file = Some(a),
            other => {
                eprintln!("error: bad dashboard arg: {other}");
                return 2;
            }
        }
    }
    let Some(file) = file else {
        eprintln!("error: dashboard needs a session FILE [--redact]");
        return 2;
    };
    match load_session_file(file) {
        Ok(s) => {
            let text = dashboard::render_text(&s);
            print!("{}", redact_output(&text, redact, &[&s]));
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

/// ASCII power timeline with event markers + interval summary.
fn cmd_timeline(args: &[String]) -> i32 {
    let mut file: Option<String> = None;
    let mut width = 60usize;
    let mut redact = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--width" => {
                i += 1;
                width = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &usize| *v > 0)
                    .unwrap_or(0);
                if width == 0 {
                    eprintln!("error: invalid --width (positive columns)");
                    return 2;
                }
            }
            "--redact" => redact = true,
            other if !other.starts_with("--") && file.is_none() => file = Some(other.to_string()),
            other => {
                eprintln!("error: bad timeline arg: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("error: timeline needs a session FILE [--width N] [--redact]");
        return 2;
    };
    match load_session_file(&file) {
        Ok(s) => {
            let text = timeline::render_ascii(&s, width);
            print!("{}", redact_output(&text, redact, &[&s]));
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

/// Per-collector freshness derived from stored timestamps + repeated values.
fn cmd_freshness(args: &[String]) -> i32 {
    if args.len() != 1 || args[0].starts_with("--") {
        eprintln!("error: freshness needs exactly one session FILE");
        return 2;
    }
    match load_session_file(&args[0]) {
        Ok(s) => {
            print!("{}", dashboard::render_freshness(&s));
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

/// Newest .jsonl session in DIR by mtime, if any.
fn latest_jsonl(dir: &str) -> Option<String> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map(|e| {
            e.filter_map(|x| x.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    if files.is_empty() {
        return None;
    }
    files.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });
    files.last().map(|p| p.to_string_lossy().to_string())
}

/// Named-pipe IPC (Phase B): `status --pipe` / `send CMD [ARG] --pipe` query
/// the agent pipe; `serve --pipe [FILE]` answers ONE request from a session
/// snapshot then exits (transport proof; the Phase C daemon keeps serving).
fn cmd_ipc_pipe(rest: &[String], dir: &str) -> i32 {
    let ask = |req: &str| match ipc_server::query(&ipc_server::default_pipe_name(), req) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    };
    match rest.first().map(|s| s.as_str()) {
        Some("status") => ask(&ipc::encode_request("status", None)),
        Some("send") => {
            let (Some(cmd), arg) = (rest.get(1), rest.get(2).cloned()) else {
                eprintln!("error: ipc send needs a CMD [ARG] (see usage)");
                return 2;
            };
            if !ipc::is_valid_verb(cmd) {
                eprintln!("error: unknown command: {cmd}");
                return 2;
            }
            let arg_json = arg.map(|a| format!("\"{}\"", pf_core::telemetry::escape_json(&a)));
            ask(&ipc::encode_request(cmd, arg_json.as_deref()))
        }
        Some("serve") => {
            let path = match rest.get(1).cloned().or_else(|| latest_jsonl(dir)) {
                Some(p) => p,
                None => {
                    eprintln!("error: no .jsonl sessions in {dir}");
                    return 1;
                }
            };
            let s = match load_session_file(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    return 2;
                }
            };
            let snap = service::snapshot_from_sessiondata(&s);
            let name = ipc_server::default_pipe_name();
            println!(
                "ipc pipe serving {path} on {name} (one request; Phase C daemon keeps serving)"
            );
            let snap2 = snap.clone();
            match ipc_server::serve_once(&name, snap, move |req| match ipc::decode_request(req) {
                Ok((cmd, arg)) => ipc_server::dispatch(&cmd, arg.as_deref(), &snap2),
                Err(e) => ipc::encode_response_err(&e),
            }) {
                Ok(()) => {
                    println!("ipc pipe served one request");
                    0
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    1
                }
            }
        }
        _ => {
            eprintln!("error: ipc needs serve|status|send (see usage)");
            2
        }
    }
}

/// File-based IPC: `serve` snapshots state.json, `status` prints it,
/// `send` writes a command file for a (future) live monitor to consume.
/// With `--pipe`, the same verbs go over the named-pipe transport instead.
fn cmd_ipc(args: &[String]) -> i32 {
    let mut dir = "sessions".to_string();
    let mut pipe = false;
    // Extract --dir/--pipe wherever they appear; the rest is the subcommand.
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--dir" {
            i += 1;
            match args.get(i) {
                Some(d) => dir = d.clone(),
                None => {
                    eprintln!("error: missing --dir value");
                    return 2;
                }
            }
        } else if args[i] == "--pipe" {
            pipe = true;
        } else {
            rest.push(args[i].clone());
        }
        i += 1;
    }
    if pipe {
        return cmd_ipc_pipe(&rest, &dir);
    }
    match rest.first().map(|s| s.as_str()) {
        Some("serve") => {
            let Some(path) = latest_jsonl(&dir) else {
                eprintln!("error: no .jsonl sessions in {dir}");
                return 1;
            };
            let s = match load_session_file(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    return 2;
                }
            };
            let state = ipc::state_from_session(&s);
            let state_path = ipc::state_path(&dir);
            if let Some(parent) = std::path::Path::new(&state_path).parent()
                && std::fs::create_dir_all(parent).is_err()
            {
                eprintln!("error: cannot create ipc dir");
                return 1;
            }
            match std::fs::write(&state_path, &state) {
                Ok(()) => {
                    println!("ipc state: {path} -> {state_path}");
                    0
                }
                Err(e) => {
                    eprintln!("error: cannot write {state_path}: {e}");
                    1
                }
            }
        }
        Some("status") => match std::fs::read_to_string(ipc::state_path(&dir)) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(_) => {
                eprintln!("error: no ipc state (run `ipc serve` first)");
                1
            }
        },
        Some("send") => {
            let (Some(cmd), arg) = (rest.get(1), rest.get(2).cloned()) else {
                eprintln!("error: ipc send needs a CMD (status|snapshot|start|stop|marker) [ARG]");
                return 2;
            };
            match ipc::encode_command(cmd, arg.as_deref(), now_wall_ms()) {
                Ok(body) => {
                    let cdir = ipc::commands_dir(&dir);
                    if std::fs::create_dir_all(&cdir).is_err() {
                        eprintln!("error: cannot create {cdir}");
                        return 1;
                    }
                    let cpath = ipc::command_path(&dir, cmd);
                    match std::fs::write(&cpath, &body) {
                        Ok(()) => {
                            println!("ipc command {cmd} -> {cpath}");
                            0
                        }
                        Err(e) => {
                            eprintln!("error: cannot write {cpath}: {e}");
                            1
                        }
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    2
                }
            }
        }
        _ => {
            eprintln!("error: ipc needs serve|status|send (see usage)");
            2
        }
    }
}

/// Headless service client (Phase C): `service
/// (status|start|stop|pause|resume|marker) [ARG] [--pipe]`. With `--pipe` the
/// verb goes over the agent named pipe (same query path as `ipc --pipe`);
/// without `--pipe` there is no daemon to talk to, so report that honestly
/// (exit 1) with the file fallback instead of faking a reply.
fn cmd_service(args: &[String]) -> i32 {
    let mut pipe = false;
    let mut rest: Vec<String> = Vec::new();
    for a in args {
        if a == "--pipe" {
            pipe = true;
        } else {
            rest.push(a.clone());
        }
    }
    let Some(cmd) = rest.first().cloned() else {
        eprintln!("error: service needs status|stop|pause|resume|marker [--pipe] (see usage)");
        return 2;
    };
    if !["status", "stop", "pause", "resume", "marker"].contains(&cmd.as_str()) {
        eprintln!("error: unknown service command: {cmd} (status|stop|pause|resume|marker)");
        return 2;
    }
    let needs_arg = cmd == "marker";
    let arg = rest.get(1).cloned();
    if needs_arg && arg.is_none() {
        eprintln!("error: service {cmd} needs an ARG (label/text)");
        return 2;
    }
    if !needs_arg && arg.is_some() {
        eprintln!("error: service {cmd} takes no ARG");
        return 2;
    }
    if rest.len() > 2 {
        eprintln!("error: too many service args (see usage)");
        return 2;
    }
    if !pipe {
        eprintln!(
            "agent not attached (Phase C daemon pending); file fallback: \
             run `power-forensics ipc status --dir sessions` after `ipc serve`, \
             or list sessions/ directly"
        );
        return 1;
    }
    let arg_json = arg.map(|a| format!("\"{}\"", pf_core::telemetry::escape_json(&a)));
    let req = ipc::encode_request(&cmd, arg_json.as_deref());
    match ipc_server::query(&ipc_server::default_pipe_name(), &req) {
        Ok(text) => match ipc::decode_response(&text) {
            Ok(data) => {
                if cmd == "status" {
                    match pf_core::json::parse(&data) {
                        Ok(v) => {
                            let s = |k: &str| {
                                v.get(k)
                                    .map(pf_core::json::to_json)
                                    .unwrap_or_else(|| "-".to_string())
                            };
                            println!(
                                "agent running={} paused={} label={} uptime_ms={} markers_accepted={}",
                                s("running"),
                                s("paused"),
                                s("label"),
                                s("uptime_ms"),
                                s("markers_accepted"),
                            );
                            println!("{data}");
                        }
                        Err(_) => println!("{data}"),
                    }
                } else {
                    println!("{data}");
                }
                0
            }
            Err(e) => {
                eprintln!("agent error: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// Crash recovery: append a reconstructed footer to a COPY
/// (`FILE.recovered.jsonl`); the original is never modified.
fn cmd_recover(args: &[String]) -> i32 {
    if args.len() != 1 || args[0].starts_with("--") {
        eprintln!("error: recover needs exactly one session FILE");
        return 2;
    }
    let file = &args[0];
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: cannot read {file}: {e}");
            return 2;
        }
    };
    match store::recover_session_text(&text, now_wall_ms()) {
        Ok(recovered) => {
            let out = format!("{file}.recovered.jsonl");
            if std::path::Path::new(&out).exists() {
                eprintln!("error: refusing to overwrite existing {out}");
                return 1;
            }
            match std::fs::write(&out, &recovered) {
                Ok(()) => {
                    println!("recovered {file} -> {out} (footer reconstructed, records preserved)");
                    0
                }
                Err(e) => {
                    eprintln!("error: cannot write {out}: {e}");
                    1
                }
            }
        }
        Err(e) => {
            eprintln!("error: cannot recover {file}: {e}");
            1
        }
    }
}

fn parse_monitor_args(args: &[String]) -> Result<MonitorOpts, String> {
    let mut opts = MonitorOpts {
        interval_ms: 1000,
        samples: None,
        out: None,
        note: String::new(),
        helper: None,
        helper_every: 60,
        collectors: None,
        preset: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--interval-ms" => {
                i += 1;
                opts.interval_ms = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &u64| *v >= 100 && *v <= 60_000)
                    .ok_or("invalid --interval-ms (100..60000)")?;
            }
            "--samples" => {
                i += 1;
                opts.samples = Some(
                    args.get(i)
                        .and_then(|s| s.parse().ok())
                        .filter(|v: &u64| *v > 0)
                        .ok_or("invalid --samples")?,
                );
            }
            "--out" => {
                i += 1;
                opts.out = Some(args.get(i).cloned().ok_or("missing --out value")?);
            }
            "--note" => {
                i += 1;
                opts.note = args.get(i).cloned().ok_or("missing --note value")?;
            }
            "--helper" => {
                i += 1;
                opts.helper = Some(args.get(i).cloned().ok_or("missing --helper value")?);
            }
            "--helper-every" => {
                i += 1;
                opts.helper_every = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .filter(|v: &u64| *v >= 10 && *v <= 3600)
                    .ok_or("invalid --helper-every (10..3600 seconds)")?;
            }
            "--collectors" => {
                i += 1;
                opts.collectors = Some(args.get(i).cloned().ok_or("missing --collectors value")?);
            }
            "--preset" => {
                i += 1;
                let p = args.get(i).cloned().ok_or("missing --preset value")?;
                if !["normal", "low", "deep"].contains(&p.as_str()) {
                    return Err("invalid --preset (normal|low|deep)".to_string());
                }
                opts.preset = Some(p);
            }
            other => return Err(format!("unknown arg: {other}")),
        }
        i += 1;
    }
    Ok(opts)
}

#[cfg(windows)]
#[link(name = "shell32")]
unsafe extern "system" {
    fn IsUserAnAdmin() -> i32;
}

/// Real elevation check (shell32), matching `pf-elevated`'s gate. Non-Windows
/// is never elevated: this is a Windows-first tool.
fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        // SAFETY: argument-free getter.
        unsafe { IsUserAnAdmin() != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// `capabilities` (no flag) prints the raw JSON unchanged for automation.
/// `capabilities --human` renders each collector/field as AVAILABLE /
/// UNAVAILABLE / DEGRADED with the reason and whether elevation would help.
///
/// The classification is honest rather than optimistic: a field whose
/// declared provenance is `unavailable` is UNAVAILABLE regardless of
/// privilege (e.g. NVMe power on this drive); a field that `requires_admin`
/// is UNAVAILABLE when the process is not elevated and AVAILABLE when it is.
/// A collector with a mix is DEGRADED. It is a capability *inventory*, not a
/// live probe, so a physically-absent optional sensor is out of scope.
fn render_capabilities_human(json: &str, elevated: bool) -> Result<String, String> {
    let v = pf_core::json::parse(json).map_err(|e| format!("bad capabilities JSON: {e}"))?;
    let collectors = v
        .get("collectors")
        .and_then(|c| c.arr())
        .ok_or("capabilities JSON lacks \"collectors\"".to_string())?;
    let mut out = format!(
        "power-forensics capabilities  (elevated: {})\n",
        if elevated { "yes" } else { "no" }
    );
    let (mut n_avail, mut n_degraded, mut n_unavail) = (0usize, 0usize, 0usize);
    for c in collectors {
        let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let fields = c.get("fields").and_then(|f| f.arr()).unwrap_or(&[]);
        let mut available = 0usize;
        let mut unavailable = 0usize;
        let mut field_lines = String::new();
        for f in fields {
            let fname = f.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let unit = f.get("unit").and_then(|n| n.as_str()).unwrap_or("");
            let source = f.get("source").and_then(|n| n.as_str()).unwrap_or("");
            let notes = f.get("notes").and_then(|n| n.as_str()).unwrap_or("");
            let prov = f.get("provenance").and_then(|n| n.as_str()).unwrap_or("");
            let admin = f
                .get("requires_admin")
                .and_then(|n| n.as_bool())
                .unwrap_or(false);
            if prov == "unavailable" {
                unavailable += 1;
                field_lines.push_str(&format!(
                    "  {fname}  UNAVAILABLE  {unit}  [{source}]  reason: {notes}  \
                     (elevation would help: {})\n",
                    if admin { "yes" } else { "no" }
                ));
            } else if admin && !elevated {
                unavailable += 1;
                field_lines.push_str(&format!(
                    "  {fname}  UNAVAILABLE  {unit}  [{source}]  \
                     reason: requires administrator privileges  (elevation would help: yes)\n"
                ));
            } else {
                available += 1;
                field_lines.push_str(&format!("  {fname}  AVAILABLE  {unit}  [{source}]\n"));
            }
        }
        let status = if unavailable == 0 {
            n_avail += 1;
            "AVAILABLE"
        } else if available == 0 {
            n_unavail += 1;
            "UNAVAILABLE"
        } else {
            n_degraded += 1;
            "DEGRADED"
        };
        out.push_str(&format!(
            "{name}  {status}  ({available} available, {unavailable} unavailable)\n{field_lines}"
        ));
    }
    out.push_str(&format!(
        "collectors: {n_avail} available, {n_degraded} degraded, {n_unavail} unavailable\n"
    ));
    Ok(out)
}

fn cmd_capabilities(args: &[String]) -> i32 {
    let mut human = false;
    for a in args {
        match a.as_str() {
            "--human" => human = true,
            other => {
                eprintln!("error: bad capabilities arg: {other} (usage: capabilities [--human])");
                return 2;
            }
        }
    }
    let json = capabilities_json();
    if !human {
        println!("{json}");
        return 0;
    }
    match render_capabilities_human(&json, is_elevated()) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

/// Persistent agent daemon (Phase C): one `Service` owns the single
/// authoritative lifecycle. `ipc_server::serve` runs on a thread against a
/// `Service` clone while `cmd_monitor_with` drives the writer loop on the
/// main thread with the live-snapshot and pause slots, so `status`/`marker`/
/// `pause`/`resume` stay reachable without an attached CLI and `pause`
/// actually freezes recording. The monitor is driven with `Service`'s
/// `snapshot_handle`, `pause_flag` and `stop_flag`, so a pipe `stop`
/// (`service stop --pipe` / `ipc send stop --pipe`) is authoritative: the
/// monitor finalizes the session (flush + exactly one footer), the pipe
/// server is told to stop, and the process exits 0. The stop-file sentinel
/// and `--samples` remain independent clean-stop paths.
fn cmd_agent(args: &[String]) -> i32 {
    let mut opts = match parse_monitor_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n{usage}", usage = usage());
            return 2;
        }
    };
    let sessions_dir = opts
        .out
        .as_deref()
        .and_then(|p| std::path::Path::new(p).parent())
        .map(|p| p.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sessions".to_string());
    let label = if opts.note.is_empty() {
        "agent".to_string()
    } else {
        opts.note.clone()
    };
    // One authority for the session label: the daemon lifecycle label is
    // written both into the session header (via `opts.note`) and into the
    // live/status snapshots, so `status`, `snapshot`, and the session file
    // cannot disagree about what the run is called.
    opts.note = label.clone();
    // Single owner: at most one agent per user may own monitoring. Acquired
    // BEFORE any scheduler, collector, writer, or session file exists, so a
    // second start creates nothing and exits cleanly. A named mutex is
    // released by the OS on process exit/crash, so there is no stale PID.
    let _ownership = match ipc_server::SingleInstance::acquire(&ipc_server::agent_mutex_name()) {
        Ok(guard) => guard,
        Err(ipc_server::InstanceError::AlreadyRunning) => {
            eprintln!(
                "error: another agent already owns monitoring for this user \
                 (single-instance lock held); no session created"
            );
            return 1;
        }
        Err(e) => {
            eprintln!("error: cannot acquire agent ownership: {e}");
            return 1;
        }
    };
    let svc = service::Service::new(&sessions_dir);
    svc.start(&label);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pipe = ipc_server::default_pipe_name();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let server = {
        let name = pipe.clone();
        let svc = svc.clone();
        let stop = stop.clone();
        std::thread::spawn(move || ipc_server::serve_with_ready(&name, svc, stop, ready_tx))
    };
    // The command pipe is required: failing to establish it is fatal, so we
    // never continue as an uncontrolled headless monitor.
    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("error: agent command pipe failed to start: {e}");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = server.join();
            return 2;
        }
        Err(_) => {
            eprintln!("error: agent command pipe did not start within 5s");
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = server.join();
            return 2;
        }
    }
    println!(
        "agent: serving {pipe} as {label:?}; monitoring until `service stop --pipe`, \
         <out-dir>/stop.requested or --samples"
    );
    let code = monitor::cmd_monitor_with_control(
        opts,
        Some(svc.snapshot_handle()),
        Some(svc.pause_flag()),
        svc.stop_flag(),
        svc.marker_queue(),
        Some(svc.session_path_handle()),
    );
    svc.stop();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    match server.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("agent: pipe server stopped: {e}"),
        Err(_) => eprintln!("agent: pipe server thread panicked"),
    }
    code
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(|s| s.as_str()) {
        Some("capabilities") => cmd_capabilities(&args[1..]),
        Some("sample") => cmd_sample(),
        Some("monitor") => match parse_monitor_args(&args[1..]) {
            Ok(opts) => monitor::cmd_monitor(opts),
            Err(e) => {
                eprintln!("error: {e}\n{usage}", usage = usage());
                2
            }
        },
        Some("agent") => cmd_agent(&args[1..]),
        Some("diagnose") => cmd_diagnose(&args[1..]),
        Some("compare") => cmd_compare(&args[1..]),
        Some("report") => cmd_report(&args[1..]),
        Some("sessions") => cmd_sessions(&args[1..]),
        Some("export") => cmd_export(&args[1..]),
        Some("experiment") => cmd_experiment(&args[1..]),
        Some("overhead") => overhead::cmd_overhead(&args[1..]),
        Some("calibrate") => cmd_calibrate(&args[1..]),
        Some("store") => cmd_store(&args[1..]),
        Some("dashboard") => cmd_dashboard(&args[1..]),
        Some("timeline") => cmd_timeline(&args[1..]),
        Some("freshness") => cmd_freshness(&args[1..]),
        Some("ipc") => cmd_ipc(&args[1..]),
        Some("service") => cmd_service(&args[1..]),
        Some("recover") => cmd_recover(&args[1..]),
        _ => {
            eprintln!("{}", usage());
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_json_lists_all_collectors() {
        let j = capabilities_json();
        for c in [
            "battery", "cpu", "gpu", "proc", "display", "net", "storage", "usb", "os_power",
        ] {
            assert!(
                j.contains(&format!("\"name\":\"{c}\"")),
                "missing collector {c}"
            );
        }
        // Must be parseable, not just string-shaped.
        assert!(pf_core::json::parse(&j).is_ok());
    }

    #[test]
    fn human_capabilities_render_available_degraded_unavailable() {
        let json = r#"{"collectors":[
            {"name":"battery","fields":[
                {"name":"battery.discharge_w","unit":"W","source":"GetSystemPowerStatus",
                 "provenance":"measured","interval_ms":1000,"requires_admin":false,"notes":"x"}
            ]},
            {"name":"storage","fields":[
                {"name":"storage.disk.util_pct","unit":"%","source":"PDH",
                 "provenance":"measured","interval_ms":1000,"requires_admin":false,"notes":"x"},
                {"name":"storage.nvme_power","unit":"state","source":"NVMe IOCTL",
                 "provenance":"measured","interval_ms":0,"requires_admin":true,"notes":"needs admin"}
            ]},
            {"name":"nvme","fields":[
                {"name":"nvme.health","unit":"%","source":"none",
                 "provenance":"unavailable","interval_ms":0,"requires_admin":true,"notes":"absent on this drive"},
                {"name":"nvme.temp","unit":"C","source":"none",
                 "provenance":"unavailable","interval_ms":0,"requires_admin":false,"notes":"no sensor"}
            ]}
        ]}"#;
        let no = render_capabilities_human(json, false).unwrap();
        assert!(no.contains("battery  AVAILABLE"), "{no}");
        assert!(no.contains("storage  DEGRADED"), "{no}");
        assert!(no.contains("storage.nvme_power  UNAVAILABLE"), "{no}");
        assert!(no.contains("storage.nvme_power  UNAVAILABLE  state  [NVMe IOCTL]  reason: requires administrator privileges  (elevation would help: yes)"), "{no}");
        assert!(no.contains("nvme  UNAVAILABLE"), "{no}");
        assert!(
            no.contains(
                "nvme.temp  UNAVAILABLE  C  [none]  reason: no sensor  (elevation would help: no)"
            ),
            "{no}"
        );
        assert!(
            no.contains("collectors: 1 available, 1 degraded, 1 unavailable"),
            "{no}"
        );

        let yes = render_capabilities_human(json, true).unwrap();
        assert!(yes.contains("storage  AVAILABLE"), "{yes}");
        assert!(yes.contains("(elevated: yes)"), "{yes}");
        // A permanently-unavailable field stays unavailable with elevation.
        assert!(yes.contains("nvme  UNAVAILABLE"), "{yes}");

        // The shipped JSON renders through the same path.
        let real = capabilities_json();
        let rendered = render_capabilities_human(&real, false).unwrap();
        assert!(rendered.contains("battery  "), "{rendered}");
        assert!(render_capabilities_human("not json", false).is_err());
    }

    #[test]
    fn capabilities_rejects_bad_args() {
        assert_eq!(cmd_capabilities(&["--bogus".to_string()]), 2);
        assert_eq!(cmd_capabilities(&[]), 0);
    }

    #[test]
    fn rejects_bad_monitor_args() {
        assert!(parse_monitor_args(&["--interval-ms".to_string()]).is_err());
        assert!(parse_monitor_args(&["--interval-ms".to_string(), "5".to_string()]).is_err());
        assert!(parse_monitor_args(&["--bogus".to_string()]).is_err());
        let ok = parse_monitor_args(&[
            "--interval-ms".to_string(),
            "2000".to_string(),
            "--samples".to_string(),
            "3".to_string(),
        ])
        .unwrap();
        assert_eq!(ok.interval_ms, 2000);
        assert_eq!(ok.samples, Some(3));
    }

    #[test]
    fn analysis_commands_reject_bad_args() {
        assert_eq!(cmd_diagnose(&[]), 2);
        assert_eq!(cmd_diagnose(&["--baseline".to_string()]), 2);
        assert_eq!(cmd_diagnose(&["no-such-file.jsonl".to_string()]), 2);
        assert_eq!(cmd_compare(&[]), 2);
        assert_eq!(cmd_compare(&["only-one.jsonl".to_string()]), 2);
        assert_eq!(cmd_report(&[]), 2);
        assert_eq!(cmd_report(&["a".to_string(), "b".to_string()]), 2);
    }

    #[test]
    fn productization_commands_reject_bad_args() {
        assert_eq!(cmd_dashboard(&[]), 2);
        assert_eq!(cmd_dashboard(&["no-such-file.jsonl".to_string()]), 2);
        assert_eq!(cmd_timeline(&[]), 2);
        assert_eq!(
            cmd_timeline(&["--width".to_string(), "0".to_string(), "f".to_string()]),
            2
        );
        assert_eq!(cmd_freshness(&[]), 2);
        assert_eq!(cmd_ipc(&[]), 2);
        assert_eq!(cmd_ipc(&["send".to_string()]), 2);
        assert_eq!(cmd_ipc(&["send".to_string(), "reboot".to_string()]), 2);
        assert_eq!(cmd_recover(&[]), 2);
        assert_eq!(cmd_recover(&["no-such-file.jsonl".to_string()]), 2);
        // Retention/storage flags validate their values.
        assert_eq!(cmd_sessions(&["--max-mb".to_string()]), 2);
        assert_eq!(cmd_sessions(&["--max-mb".to_string(), "0".to_string()]), 2);
        assert_eq!(cmd_store(&[]), 2);
        assert_eq!(cmd_store(&["--bogus".to_string()]), 2);
    }

    #[test]
    fn ipc_pipe_rejects_bad_args_without_touching_pipe() {
        assert_eq!(cmd_ipc(&["--pipe".to_string()]), 2);
        assert_eq!(cmd_ipc(&["send".to_string(), "--pipe".to_string()]), 2);
        assert_eq!(
            cmd_ipc(&[
                "send".to_string(),
                "reboot".to_string(),
                "--pipe".to_string()
            ]),
            2
        );
        assert_eq!(
            cmd_ipc(&[
                "serve".to_string(),
                "--pipe".to_string(),
                "no-such-file.jsonl".to_string()
            ]),
            2
        );
    }

    #[test]
    fn ipc_pipe_snapshot_takes_last_point() {
        let text = "{\"collector\":\"battery\",\"wall_ms\":1,\"mono_ms\":0,\"discharge_w\":{\"v\":5.0,\"p\":\"measured\"}}\n\
                    {\"collector\":\"battery\",\"wall_ms\":2,\"mono_ms\":1000,\"discharge_w\":{\"v\":7.0,\"p\":\"measured\"}}\n";
        let s = session::load_session("t", text).unwrap();
        // The builder is the single service implementation; the CLI copy was
        // deleted (Task: dedupe snapshot_from_session).
        let snap = service::snapshot_from_sessiondata(&s);
        assert_eq!(snap.watts, Some(7.0));
        assert_eq!(snap.recent, vec![Some(5.0), Some(7.0)]);
        assert_eq!(snap.label, "t");
    }

    #[test]
    fn agent_rejects_bad_args_before_starting() {
        assert_eq!(cmd_agent(&["--bogus".to_string()]), 2);
        assert_eq!(
            cmd_agent(&["--interval-ms".to_string(), "5".to_string()]),
            2
        );
        assert_eq!(cmd_agent(&["--preset".to_string(), "turbo".to_string()]), 2);
    }

    #[test]
    fn service_pause_resume_take_no_arg() {
        // Extra ARG is rejected before any pipe access.
        assert_eq!(
            cmd_service(&[
                "pause".to_string(),
                "extra".to_string(),
                "--pipe".to_string()
            ]),
            2
        );
        assert_eq!(
            cmd_service(&[
                "resume".to_string(),
                "extra".to_string(),
                "--pipe".to_string()
            ]),
            2
        );
        // Without --pipe there is no daemon; honest exit 1, never a fake reply.
        assert_eq!(cmd_service(&["pause".to_string()]), 1);
        assert_eq!(cmd_service(&["resume".to_string()]), 1);
    }

    #[test]
    fn service_rejects_bad_args_without_touching_pipe() {
        assert_eq!(cmd_service(&[]), 2);
        assert_eq!(cmd_service(&["bogus".to_string(), "--pipe".to_string()]), 2);
        assert_eq!(cmd_service(&["start".to_string(), "--pipe".to_string()]), 2);
        assert_eq!(
            cmd_service(&["marker".to_string(), "--pipe".to_string()]),
            2
        );
        assert_eq!(
            cmd_service(&[
                "status".to_string(),
                "extra".to_string(),
                "--pipe".to_string()
            ]),
            2
        );
        assert_eq!(
            cmd_service(&[
                "status".to_string(),
                "a".to_string(),
                "b".to_string(),
                "--pipe".to_string()
            ]),
            2
        );
    }

    #[test]
    fn service_without_pipe_is_honest_exit_1() {
        assert_eq!(cmd_service(&["status".to_string()]), 1);
        assert_eq!(cmd_service(&["marker".to_string(), "demo".to_string()]), 1);
    }

    /// A synthetic condition block for the repeated-experiment dispatch.
    fn synth_block(label: &str, watts: f64, cpu: f64, coverage: Option<f64>) -> ExpBlock {
        use pf_core::telemetry::{ClockStamp, Telemetry};
        let st = ClockStamp {
            wall_millis: 0,
            mono_millis: 0,
        };
        let mut s = session::SessionData {
            label: label.to_string(),
            ..Default::default()
        };
        s.battery_meta.full_mwh = Some(50_000.0);
        for i in 0..60 {
            let t = i as f64;
            // Tiny variation so this looks like a real (not cached) sensor.
            let w = watts + (i % 5) as f64 * 0.01;
            s.battery.push(session::BatteryPoint {
                t,
                discharge: Telemetry::measured(w, "battery", st),
                remaining_wh: Telemetry::measured(30.0, "battery", st),
                ..Default::default()
            });
            s.cpu.push(session::CpuPoint {
                t,
                utility: Telemetry::measured(cpu, "cpu", st),
                ..Default::default()
            });
        }
        let (median, mut quality) = analysis::block_median(&s);
        quality.coverage = coverage;
        let cpu_median = analysis::block_cpu_median(&s);
        ExpBlock {
            median,
            quality,
            cpu_median,
            session: s,
        }
    }

    #[test]
    fn experiment_settle_routes_through_trim() {
        use pf_core::telemetry::{ClockStamp, Telemetry};
        let st = ClockStamp {
            wall_millis: 0,
            mono_millis: 0,
        };
        let mut s = session::SessionData {
            label: "s".to_string(),
            ..Default::default()
        };
        // 40 s at 12 W then 20 s at 6 W. Auto-settle keeps the opening
        // majority regime; an explicit 40 s trim isolates the final 6 W.
        for i in 0..60 {
            let w = if i < 40 { 12.0 } else { 6.0 };
            s.battery.push(session::BatteryPoint {
                t: i as f64,
                discharge: Telemetry::measured(w, "battery", st),
                remaining_wh: Telemetry::measured(30.0, "battery", st),
                ..Default::default()
            });
        }
        let auto = analyze_single_experiment(&s, &s, 0);
        let trimmed = analyze_single_experiment(&s, &s, 40);
        assert_ne!(auto.med_a, trimmed.med_a);
        assert!((trimmed.med_a.unwrap() - 6.0).abs() < 1e-9);
        assert!(auto.med_a.unwrap() > trimmed.med_a.unwrap());
    }

    #[test]
    fn repeated_experiment_flags_cpu_mismatch_and_reports_runtime() {
        let a1 = synth_block("a", 6.0, 4.0, Some(1.0));
        let a2 = synth_block("a", 6.05, 4.0, Some(1.0));
        let b1 = synth_block("b", 8.0, 16.0, Some(1.0));
        let b2 = synth_block("b", 8.05, 16.0, Some(1.0));
        let c = analyze_repeated_experiment("A", "B", &[a1, a2], &[b1, b2]).unwrap();
        assert!(
            c.lines
                .iter()
                .any(|l| l.contains("background CPU mismatch")),
            "lines: {:?}",
            c.lines
        );
        assert_ne!(c.validity, analysis::Validity::Valid);
        // Runtime is reported on the same full-charge basis as reps==1.
        assert!(c.runtime_norm_a_h.is_some() && c.runtime_norm_b_h.is_some());
        assert!(c.projected_delta_min.is_some());
        let text = analysis::render_comparison(&c);
        assert!(text.contains("normalized full-charge runtime"), "{text}");
    }

    #[test]
    fn repeated_experiment_coverage_gap_is_invalid() {
        let a = synth_block("a", 6.0, 4.0, Some(1.0));
        let b = synth_block("b", 8.0, 4.0, Some(0.2));
        let c = analyze_repeated_experiment("A", "B", &[a], &[b]).unwrap();
        assert_eq!(c.validity, analysis::Validity::Invalid);
    }

    #[test]
    fn recover_writes_copy_and_never_overwrites_source() {
        let dir = std::env::temp_dir().join(format!("pf-cli-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("torn.jsonl");
        let text = "{\"type\":\"session_header\",\"wall_ms\":1000}\n\
                    {\"collector\":\"battery\",\"wall_ms\":1000,\"mono_ms\":0,\"discharge_w\":{\"v\":6.0,\"p\":\"measured\"}}\n\
                    {\"collector\":\"battery\",\"wall_ms\":2000,\"mono_ms\":1000,\"disc";
        std::fs::write(&src, text).unwrap();
        assert_eq!(cmd_recover(&[src.to_string_lossy().to_string()]), 0);
        let out = dir.join("torn.jsonl.recovered.jsonl");
        assert!(out.exists(), "recovered copy was not written");
        assert_eq!(
            std::fs::read_to_string(&src).unwrap(),
            text,
            "source must never be mutated"
        );
        // Existing recovered copy is protected: second run refuses.
        assert_eq!(cmd_recover(&[src.to_string_lossy().to_string()]), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Session fixture exercising the identifier classes the share commands
    /// render: a hostname-like label plus a process name + PID (CSV `pid:name`).
    fn redaction_fixture() -> String {
        "{\"type\":\"session_header\",\"wall_ms\":1000}\n\
         {\"collector\":\"battery\",\"wall_ms\":1000,\"mono_ms\":0,\"discharge_w\":{\"v\":6.0,\"p\":\"measured\"}}\n\
         {\"collector\":\"proc\",\"wall_ms\":1000,\"mono_ms\":0,\"top\":[{\"pid\":4321,\"name\":\"chrome.exe\",\"cpu_pct\":{\"v\":0.1,\"p\":\"derived\"}}]}\n\
         {\"type\":\"session_footer\",\"wall_ms\":2000,\"lines\":4,\"summary\":{}}\n"
            .to_string()
    }

    #[test]
    fn redaction_masks_dashboard_timeline_and_compare_but_stays_opt_in() {
        use pf_core::telemetry::{ClockStamp, Telemetry};
        let st = ClockStamp {
            wall_millis: 0,
            mono_millis: 0,
        };
        let mut s = session::SessionData {
            label: "DESKTOP-ABC123.jsonl".to_string(),
            ..Default::default()
        };
        s.battery.push(session::BatteryPoint {
            t: 0.0,
            discharge: Telemetry::measured(6.0, "battery", st),
            ..Default::default()
        });
        s.procs.push(session::ProcPoint {
            t: 0.0,
            top: vec![session::ProcEntry {
                pid: 4321,
                name: "chrome.exe".to_string(),
                cpu: Telemetry::derived(0.1, "proc", st),
                ..Default::default()
            }],
            ..Default::default()
        });

        let dash = dashboard::render_text(&s);
        let red = redact_output(&dash, true, &[&s]);
        assert!(!red.contains("DESKTOP-ABC123"), "{red}");
        assert!(red.contains("<redacted>"), "{red}");
        assert_eq!(
            redact_output(&dash, false, &[&s]),
            dash,
            "redaction must be opt-in"
        );

        let tl = timeline::render_ascii(&s, 20);
        let red = redact_output(&tl, true, &[&s]);
        assert!(!red.contains("DESKTOP-ABC123"), "{red}");
        assert!(red.contains("t=0s..0s"), "structure lost: {red}");
        assert_eq!(
            redact_output(&tl, false, &[&s]),
            tl,
            "redaction must be opt-in"
        );

        // diagnose: the DIAGNOSIS header carries the label.
        let diag = analysis::render_diagnosis(&s.label, &analysis::diagnose_session(&s, None));
        let red = redact_output(&diag, true, &[&s]);
        assert!(!red.contains("DESKTOP-ABC123"), "{red}");
        assert_eq!(
            redact_output(&diag, false, &[&s]),
            diag,
            "redaction must be opt-in"
        );

        // Compare: both header labels and the differing process names are masked.
        let mut b = s.clone();
        b.label = "OTHER-HOST.jsonl".to_string();
        b.procs[0].top[0].pid = 99;
        b.procs[0].top[0].name = "edge.exe".to_string();
        let c = analysis::compare_sessions(&s, &b);
        let text = analysis::render_comparison(&c);
        assert!(
            text.contains("chrome.exe") && text.contains("edge.exe"),
            "{text}"
        );
        let red = redact_output(&text, true, &[&s, &b]);
        for secret in ["DESKTOP-ABC123", "OTHER-HOST", "chrome.exe", "edge.exe"] {
            assert!(!red.contains(secret), "leaked {secret:?} in:\n{red}");
        }
        assert_eq!(
            redact_output(&text, false, &[&s, &b]),
            text,
            "redaction must be opt-in"
        );
    }

    #[test]
    fn export_streams_matches_builder_masks_pids_and_guards_out() {
        let dir = std::env::temp_dir().join(format!("pf-cli-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("DESKTOP-ABC123.jsonl");
        std::fs::write(&src, redaction_fixture()).unwrap();
        let src_s = src.to_string_lossy().to_string();
        let s = load_session_file(&src_s).unwrap();

        let out = dir.join("data.csv");
        let out_s = out.to_string_lossy().to_string();
        // Streaming output is byte-identical to the in-memory builder.
        assert_eq!(
            cmd_export(&[src_s.clone(), "--out".to_string(), out_s.clone()]),
            0
        );
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            session::csv_text(&s)
        );

        // Default refuses to overwrite and leaves the existing file untouched.
        let before = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            cmd_export(&[src_s.clone(), "--out".to_string(), out_s.clone()]),
            1
        );
        assert_eq!(std::fs::read_to_string(&out).unwrap(), before);

        // --force overwrites.
        assert_eq!(
            cmd_export(&[
                src_s.clone(),
                "--out".to_string(),
                out_s.clone(),
                "--force".to_string()
            ]),
            0
        );

        // --redact masks the process name and the CSV `pid:name` token.
        let red = dir.join("red.csv");
        let red_s = red.to_string_lossy().to_string();
        assert_eq!(
            cmd_export(&[
                src_s.clone(),
                "--out".to_string(),
                red_s.clone(),
                "--redact".to_string()
            ]),
            0
        );
        let redtext = std::fs::read_to_string(&red).unwrap();
        assert!(!redtext.contains("chrome.exe"), "{redtext}");
        assert!(!redtext.contains("4321"), "{redtext}");
        assert!(redtext.starts_with(session::CSV_HEADER), "{redtext}");
        assert!(
            redtext
                .lines()
                .any(|l| l.starts_with("0.000,proc,cpu_pct,")),
            "structure lost:\n{redtext}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn redaction_masks_bare_device_identity_in_compare() {
        use pf_core::telemetry::{ClockStamp, Telemetry};
        let st = ClockStamp {
            wall_millis: 0,
            mono_millis: 0,
        };
        let mut s = session::SessionData {
            label: "s.jsonl".to_string(),
            ..Default::default()
        };
        for i in 0..5 {
            s.battery.push(session::BatteryPoint {
                t: i as f64,
                discharge: Telemetry::measured(6.0, "battery", st),
                ..Default::default()
            });
        }
        let gpu = session::GpuAdapterPoint {
            name: "AMD Radeon (TM) Graphics".to_string(),
            total_util: Telemetry::measured(10.0, "gpu", st),
            ..Default::default()
        };
        s.gpu.push(session::GpuPoint {
            t: 0.0,
            adapters: vec![gpu],
            ..Default::default()
        });
        let mut b = s.clone();
        b.label = "b.jsonl".to_string();
        let text = analysis::render_comparison(&analysis::compare_sessions(&s, &b));
        // The bare `GPU <name>` line has no `key:` syntax, which is exactly
        // the case keyed masking missed.
        assert!(
            text.contains("AMD Radeon"),
            "fixture did not render GPU: {text}"
        );
        let red = redact_output(&text, true, &[&s, &b]);
        assert!(
            !red.contains("AMD Radeon"),
            "bare device identity leaked:\n{red}"
        );
    }

    #[test]
    fn store_refuses_overwrite_without_force_and_preserves_existing() {
        let dir = std::env::temp_dir().join(format!("pf-cli-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.jsonl");
        // Include a real footer so migrate has a session to archive.
        std::fs::write(
            &src,
            "{\"type\":\"session_header\",\"wall_ms\":1000}\n\
             {\"collector\":\"battery\",\"wall_ms\":1000,\"mono_ms\":0,\"discharge_w\":{\"v\":6.0,\"p\":\"measured\"}}\n\
             {\"type\":\"session_footer\",\"wall_ms\":2000,\"lines\":3,\"summary\":{\"discharge_wh\":0.01}}\n",
        )
        .unwrap();
        let out = dir.join("arch");
        let out_s = out.to_string_lossy().to_string();
        let src_s = src.to_string_lossy().to_string();

        assert_eq!(
            cmd_store(&[src_s.clone(), "--out".to_string(), out_s.clone()]),
            0
        );
        let samples = out.join("samples.bin");
        let before = std::fs::read(&samples).unwrap();
        assert!(!before.is_empty());

        // Second run without --force refuses and leaves the archive untouched.
        assert_eq!(
            cmd_store(&[src_s.clone(), "--out".to_string(), out_s.clone()]),
            1
        );
        assert_eq!(std::fs::read(&samples).unwrap(), before);

        // --force replaces it.
        assert_eq!(
            cmd_store(&[
                src_s.clone(),
                "--out".to_string(),
                out_s.clone(),
                "--force".to_string()
            ]),
            0
        );
        assert!(!std::fs::read(&samples).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_prune_refreshes_between_policies_and_prunes_old() {
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("pf-cli-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // One old file (ages out) and one fresh file (must survive).
        let old = dir.join("old.jsonl");
        let fresh = dir.join("fresh.jsonl");
        for (path, age_days) in [(&old, 30u64), (&fresh, 0u64)] {
            let mut f = std::fs::File::create(path).unwrap();
            f.write_all(redaction_fixture().as_bytes()).unwrap();
            f.sync_all().unwrap();
            let when = SystemTime::now() - Duration::from_secs(age_days * 86_400);
            f.set_times(std::fs::FileTimes::new().set_modified(when))
                .unwrap();
        }
        let dir_s = dir.to_string_lossy().to_string();
        // Two policies in one invocation: --prune-days deletes the old file,
        // then --max-mb must re-list and see only the survivor.
        assert_eq!(
            cmd_sessions(&[
                dir_s.clone(),
                "--prune-days".to_string(),
                "5".to_string(),
                "--yes".to_string(),
                "--max-mb".to_string(),
                "1".to_string(),
            ]),
            0
        );
        assert!(!old.exists(), "old file should have been pruned");
        assert!(fresh.exists(), "fresh file must survive both policies");
        // Re-listing after deletion reflects the current disk, not the stale
        // pre-delete snapshot.
        let listed = list_jsonl(&dir_s).unwrap();
        assert_eq!(listed.len(), 1, "stale listing: {listed:?}");
        assert_eq!(listed[0].file_name().unwrap(), "fresh.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
