//! Pure dashboard + freshness rendering over `SessionData`.
//!
//! No IO here; `main.rs` loads the session and prints the returned strings.

use crate::analysis::headline_health;
use crate::session::SessionData;
use crate::stats;
use crate::telemetry::Provenance;

/// Provenance tag appended to a card value only when it is not measured.
fn prov_tag(p: Provenance) -> &'static str {
    match p {
        Provenance::Measured => "",
        Provenance::Derived => " [derived]",
        Provenance::Estimated => " [estimated]",
        Provenance::Unavailable => " [unavailable]",
    }
}

/// Same, for provenance carried as the string vocabulary of the session model.
fn prov_tag_str(p: &str) -> &'static str {
    match p {
        "measured" => "",
        "derived" => " [derived]",
        "estimated" => " [estimated]",
        _ => " [unavailable]",
    }
}

/// Numeric card field with a provenance tag for non-measured values. The tag
/// is suppressed when the value is unavailable (already labelled).
fn card(v: Option<f64>, prec: usize, unit: &str, tag: &str) -> String {
    match v {
        Some(x) => format!("{x:.prec$}{unit}{tag}", prec = prec),
        None => "[unavailable]".to_string(),
    }
}

/// One-line system-power / battery / runtime cards plus provenance lines.
pub fn render_text(s: &SessionData) -> String {
    let mut o = String::new();
    o.push_str(&format!("DASHBOARD {}\n", s.label));

    // --- SYSTEM POWER card: last + median discharge, last charge ---
    let disch: Vec<f64> = s.battery.iter().filter_map(|b| b.discharge.val()).collect();
    let last = s.battery.last();
    let last_w = last.and_then(|b| b.discharge.val());
    let last_c = last.and_then(|b| b.charge.val());
    let med = stats::median(&disch);
    let tag_now = last
        .map(|b| b.discharge.provenance())
        .unwrap_or(Provenance::Unavailable);
    let tag_chg = last
        .map(|b| b.charge.provenance())
        .unwrap_or(Provenance::Unavailable);
    o.push_str(&format!(
        "[SYSTEM POWER] now {}  charge {}  median discharge {} (n={})\n",
        card(last_w, 2, " W", prov_tag(tag_now)),
        card(last_c, 2, " W", prov_tag(tag_chg)),
        card(med, 2, " W", " [derived]"),
        disch.len()
    ));

    // --- BATTERY card ---
    let rem = last.and_then(|b| b.remaining_wh.val());
    let pct = last.and_then(|b| b.pct.val());
    let tag_rem = last
        .map(|b| b.remaining_wh.provenance())
        .unwrap_or(Provenance::Unavailable);
    let tag_pct = last
        .map(|b| b.pct.provenance())
        .unwrap_or(Provenance::Unavailable);
    let full = s.battery_meta.full_mwh;
    let design = s.battery_meta.design_mwh;
    // Health comes from the shared same-basis logic: a multi-battery aggregate
    // full-charge is never paired with one battery's design.
    let hh = headline_health(&s.battery_meta);
    let health = match hh.value {
        Some(h) => format!("{:.1}% [derived]", h * 100.0),
        None => match &hh.reason {
            Some(r) => format!("[unavailable] ({r})"),
            None => "[unavailable]".to_string(),
        },
    };
    o.push_str(&format!(
        "[BATTERY] remaining {}  charge {}  full {}  design {}  health {}\n",
        card(rem, 2, " Wh", prov_tag(tag_rem)),
        card(pct, 0, "%", prov_tag(tag_pct)),
        card(full, 0, " mWh", prov_tag_str(&s.battery_meta.full_prov)),
        card(design, 0, " mWh", prov_tag_str(&s.battery_meta.design_prov)),
        health,
    ));

    // --- RUNTIME card ---
    let rt = match (rem, med) {
        (Some(r), Some(m)) => stats::runtime_hours(r, m),
        _ => None,
    };
    o.push_str(&format!(
        "[RUNTIME] {} at median discharge\n",
        card(rt, 1, " h", " [derived]")
    ));

    // --- Provenance / freshness lines (last sample per key source) ---
    o.push_str("provenance:\n");
    if let Some(b) = last {
        o.push_str(&format!(
            "  battery.discharge_w: {} {} @{:.0}s\n",
            b.discharge.provenance().as_str(),
            b.discharge.quality.as_str(),
            b.t
        ));
    } else {
        o.push_str("  battery.discharge_w: [no samples]\n");
    }
    if let Some(c) = s.cpu.last() {
        o.push_str(&format!(
            "  cpu.utility: {} {} @{:.0}s\n",
            c.utility.provenance().as_str(),
            c.utility.quality.as_str(),
            c.t
        ));
    } else {
        o.push_str("  cpu.utility: [no samples]\n");
    }
    if let Some(d) = s.display.last() {
        o.push_str(&format!(
            "  display.brightness: {} {} @{:.0}s\n",
            d.brightness.provenance().as_str(),
            d.brightness.quality.as_str(),
            d.t
        ));
    } else {
        o.push_str("  display.brightness: [no samples]\n");
    }
    if let Some(fv) = &s.footer {
        o.push_str(&format!("footer: {}\n", crate::json::to_json(fv)));
    } else {
        o.push_str("footer: [incomplete: no session_footer recorded]\n");
    }
    o
}

// ---------------- freshness ----------------

/// Per-collector freshness derived from stored timestamps + repeated values.
/// No new collection: `observed_hz` = samples / span, `stale_age_s` = global
/// last-sample time minus this source's last time, `failures` = points where
/// the primary value was unavailable.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceFreshness {
    pub name: String,
    pub samples: usize,
    pub failures: usize,
    pub last_sampled_s: f64,
    pub last_changed_s: f64,
    pub observed_hz: f64,
    pub stale_age_s: f64,
}

fn series_stats(name: &str, pts: &[(f64, Option<f64>)], global_last: f64) -> SourceFreshness {
    let samples = pts.len();
    let failures = pts.iter().filter(|(_, v)| v.is_none()).count();
    let (first, last) = match (pts.first(), pts.last()) {
        (Some((f, _)), Some((l, _))) => (*f, *l),
        _ => {
            return SourceFreshness {
                name: name.to_string(),
                samples: 0,
                failures: 0,
                last_sampled_s: 0.0,
                last_changed_s: 0.0,
                observed_hz: 0.0,
                // Never sampled during this capture: stale for the whole span.
                stale_age_s: global_last.max(0.0),
            };
        }
    };
    let mut last_changed = first;
    for w in pts.windows(2) {
        let (t_prev, v_prev) = w[0];
        let (t_cur, v_cur) = w[1];
        let changed = match (v_prev, v_cur) {
            (Some(a), Some(b)) => (a - b).abs() > 1e-9,
            (None, None) => false,
            _ => true,
        };
        let _ = t_prev;
        if changed {
            last_changed = t_cur;
        }
    }
    let span = last - first;
    let hz = if span > 0.0 {
        samples as f64 / span
    } else {
        0.0
    };
    SourceFreshness {
        name: name.to_string(),
        samples,
        failures,
        last_sampled_s: last,
        last_changed_s: last_changed,
        observed_hz: hz,
        stale_age_s: (global_last - last).max(0.0),
    }
}

/// Compute per-source freshness for the main collectors.
pub fn freshness(s: &SessionData) -> Vec<SourceFreshness> {
    let all_last = [
        s.battery.last().map(|p| p.t),
        s.cpu.last().map(|p| p.t),
        s.gpu.last().map(|p| p.t),
        s.procs.last().map(|p| p.t),
        s.display.last().map(|p| p.t),
        s.net.last().map(|p| p.t),
        s.storage.last().map(|p| p.t),
        s.usb.last().map(|p| p.t),
        s.selfmon.last().map(|p| p.t),
    ]
    .into_iter()
    .flatten()
    .fold(0.0f64, f64::max);
    let mut out = Vec::new();
    out.push(series_stats(
        "battery",
        &s.battery
            .iter()
            .map(|p| (p.t, p.discharge.val()))
            .collect::<Vec<_>>(),
        all_last,
    ));
    out.push(series_stats(
        "cpu",
        &s.cpu
            .iter()
            .map(|p| (p.t, p.utility.val()))
            .collect::<Vec<_>>(),
        all_last,
    ));
    out.push(series_stats(
        "gpu",
        &s.gpu
            .iter()
            .map(|p| (p.t, p.adapters.first().and_then(|a| a.total_util.val())))
            .collect::<Vec<_>>(),
        all_last,
    ));
    out.push(series_stats(
        "proc",
        &s.procs
            .iter()
            .map(|p| (p.t, Some(p.top.len() as f64)))
            .collect::<Vec<_>>(),
        all_last,
    ));
    out.push(series_stats(
        "display",
        &s.display
            .iter()
            .map(|p| (p.t, p.brightness.val()))
            .collect::<Vec<_>>(),
        all_last,
    ));
    out.push(series_stats(
        "net",
        &s.net
            .iter()
            .map(|p| (p.t, p.total_rx.val()))
            .collect::<Vec<_>>(),
        all_last,
    ));
    out.push(series_stats(
        "storage",
        &s.storage
            .iter()
            .map(|p| (p.t, p.disk_time.val()))
            .collect::<Vec<_>>(),
        all_last,
    ));
    out.push(series_stats(
        "usb",
        &s.usb
            .iter()
            .map(|p| {
                (
                    p.t,
                    Some(p.counts().iter().map(|(_, n)| *n).sum::<usize>() as f64),
                )
            })
            .collect::<Vec<_>>(),
        all_last,
    ));
    out.push(series_stats(
        "self",
        &s.selfmon
            .iter()
            .map(|p| (p.t, p.cpu_pct.val()))
            .collect::<Vec<_>>(),
        all_last,
    ));
    // os_policy: change = scheme-name change; value runs as 0/1 step.
    {
        let mut pts: Vec<(f64, Option<f64>)> = Vec::new();
        let mut last_scheme: Option<&str> = None;
        let mut step = 0.0;
        for (t, pol) in &s.policy_history {
            if last_scheme != pol.scheme_name.as_deref() {
                step += 1.0;
                last_scheme = pol.scheme_name.as_deref();
            }
            pts.push((*t, Some(step)));
        }
        let mut f = series_stats("os_policy", &pts, all_last);
        // A flat scheme means "never changed during capture", not missing.
        if pts.len() > 1 && pts.windows(2).all(|w| w[0].1 == w[1].1) {
            f.last_changed_s = pts[0].0;
        }
        out.push(f);
    }
    out
}

/// Human-readable freshness table.
pub fn render_freshness(s: &SessionData) -> String {
    let mut o = String::from("SOURCE  SAMPLES  FAILS  LAST_S  CHANGED_S  HZ  STALE_S\n");
    for f in freshness(s) {
        o.push_str(&format!(
            "{}  {}  {}  {:.1}  {:.1}  {:.2}  {:.1}\n",
            f.name,
            f.samples,
            f.failures,
            f.last_sampled_s,
            f.last_changed_s,
            f.observed_hz,
            f.stale_age_s
        ));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{ClockStamp, Telemetry};

    fn stamp(ms: u64) -> ClockStamp {
        ClockStamp {
            wall_millis: ms,
            mono_millis: ms,
        }
    }

    fn session_two_points() -> SessionData {
        let mut s = SessionData {
            label: "t".to_string(),
            ..Default::default()
        };
        s.battery.push(crate::session::BatteryPoint {
            t: 0.0,
            discharge: Telemetry::measured(6.0, "battery", stamp(0)),
            remaining_wh: Telemetry::measured(20.0, "battery", stamp(0)),
            pct: Telemetry::measured(50.0, "battery", stamp(0)),
            ..Default::default()
        });
        s.battery.push(crate::session::BatteryPoint {
            t: 10.0,
            discharge: Telemetry::measured(8.0, "battery", stamp(10_000)),
            remaining_wh: Telemetry::measured(19.9, "battery", stamp(10_000)),
            pct: Telemetry::measured(49.0, "battery", stamp(10_000)),
            ..Default::default()
        });
        s.battery_meta.full_mwh = Some(40000.0);
        s.battery_meta.design_mwh = Some(42000.0);
        s
    }

    #[test]
    fn dashboard_cards_present() {
        let t = render_text(&session_two_points());
        assert!(t.contains("[SYSTEM POWER]"), "{t}");
        assert!(t.contains("[BATTERY]"), "{t}");
        assert!(t.contains("[RUNTIME]"), "{t}");
        assert!(t.contains("7.00 W"), "{t}"); // median of 6,8
        assert!(t.contains("measured"), "{t}");
    }

    #[test]
    fn dashboard_health_single_battery() {
        // 40000/42000 = 95.2% via the shared same-basis health logic.
        let t = render_text(&session_two_points());
        assert!(t.contains("health 95.2% [derived]"), "{t}");
    }

    #[test]
    fn dashboard_health_multi_battery_never_pairs_aggregate() {
        use crate::session::BatteryStaticEntry;
        let mut s = session_two_points();
        s.battery_meta.battery_count = Some(2);
        s.battery_meta.full_mwh = Some(80000.0);
        s.battery_meta.design_mwh = Some(42000.0);
        s.battery_meta.batteries = vec![
            BatteryStaticEntry {
                index: 0,
                designed_mwh: Some(42000.0),
                full_mwh: Some(38700.0),
                ..Default::default()
            },
            BatteryStaticEntry {
                index: 1,
                designed_mwh: Some(40000.0),
                full_mwh: Some(38000.0),
                ..Default::default()
            },
        ];
        let t = render_text(&s);
        // 80000/42000 = 190.5% must never be printed as a health figure.
        assert!(!t.contains("190.5%"), "{t}");
        assert!(
            t.contains("health [unavailable] (multi-battery: see per-battery)"),
            "{t}"
        );
    }

    #[test]
    fn dashboard_health_missing_design_unavailable() {
        let mut s = session_two_points();
        s.battery_meta.design_mwh = None;
        let t = render_text(&s);
        assert!(t.contains("health [unavailable]"), "{t}");
    }

    #[test]
    fn dashboard_provenance_tags_derived_and_estimated() {
        let mut s = session_two_points();
        let last = s.battery.last_mut().unwrap();
        last.discharge = Telemetry::derived(8.0, "battery", stamp(10_000));
        last.remaining_wh = Telemetry::estimated(19.9, "battery", stamp(10_000));
        last.pct = Telemetry::derived(49.0, "battery", stamp(10_000));
        let t = render_text(&s);
        // Values whose provenance differs from measured are tagged in the cards.
        assert!(t.contains("now 8.00 W [derived]"), "{t}");
        assert!(t.contains("remaining 19.90 Wh [estimated]"), "{t}");
        assert!(t.contains("charge 49% [derived]"), "{t}");
        // Derived summaries are labelled too.
        assert!(t.contains("median discharge 7.00 W [derived]"), "{t}");
        assert!(t.contains(" [derived] at median discharge"), "{t}");
    }

    #[test]
    fn freshness_derives_hz_and_stale() {
        let mut s = session_two_points();
        s.cpu.push(crate::session::CpuPoint {
            t: 0.0,
            utility: Telemetry::measured(5.0, "cpu", stamp(0)),
            ..Default::default()
        });
        // cpu sampled once at t=0 while battery runs to t=10
        let fs = freshness(&s);
        let b = fs.iter().find(|f| f.name == "battery").unwrap();
        assert_eq!(b.samples, 2);
        assert!((b.observed_hz - 0.2).abs() < 1e-9);
        assert_eq!(b.stale_age_s, 0.0);
        let c = fs.iter().find(|f| f.name == "cpu").unwrap();
        assert_eq!(c.stale_age_s, 10.0);
        let txt = render_freshness(&s);
        assert!(txt.contains("battery"), "{txt}");
    }

    #[test]
    fn repeated_values_keep_first_change_time() {
        let mut s = SessionData::default();
        for i in 0..3 {
            s.battery.push(crate::session::BatteryPoint {
                t: i as f64,
                discharge: Telemetry::measured(5.0, "battery", stamp(i * 1000)),
                ..Default::default()
            });
        }
        let b = freshness(&s)
            .into_iter()
            .find(|f| f.name == "battery")
            .unwrap();
        assert_eq!(b.last_changed_s, 0.0);
        assert_eq!(b.failures, 0);
    }
}
