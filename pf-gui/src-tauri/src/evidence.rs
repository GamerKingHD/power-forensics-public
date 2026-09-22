//! Session-model -> DTO builders.
//!
//! Pure views over `pf_core::session::SessionData` and the dashboard freshness
//! helper. No energy integration, coverage, discontinuity classification or
//! recovery logic is recomputed here; values and their provenance are carried
//! through unchanged. Unavailable readings stay `value: None`.

use std::time::{SystemTime, UNIX_EPOCH};

use pf_core::dashboard;
use pf_core::json::JVal;
use pf_core::session::SessionData;
use pf_core::telemetry::Telemetry;

use crate::dto::{
    CapabilityReport, CollectorState, DisplayInfo, EvidenceValue, FooterSummary, LiveBattery,
    LiveCpu, LiveDisplay, LiveGpuAdapter, LiveSnapshot, LiveSystem, MetricEvidence, ProcessRow,
    Series, TimelineEvent, TimelinePoint,
};

fn now_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Convert a backend reading into wire evidence. The value is never fabricated:
/// an unavailable reading has no value, only a reason and absence kind. Age is
/// left `None` here and filled by [`stamp_ages`] against the view's reference
/// time, so the same reading can be aged relative to "now" (live) or to the
/// session end (recorded) without contradiction.
pub fn ev(t: &Telemetry<f64>, unit: &str) -> EvidenceValue {
    EvidenceValue {
        value: t.val(),
        provenance: t.provenance().as_str().to_string(),
        quality: t.quality.as_str().to_string(),
        absence_kind: t.unavail_kind.map(|k| k.as_str().to_string()),
        reason: t.value.reason().map(|r| r.to_string()),
        source: t.source.to_string(),
        collector: t.source.to_string(),
        unit: unit.to_string(),
        wall_ms: t.stamp.wall_millis,
        mono_ms: t.stamp.mono_millis,
        age_ms: None,
    }
}

fn metric(key: &str, label: &str, unit: &str, e: EvidenceValue) -> MetricEvidence {
    MetricEvidence {
        key: key.to_string(),
        label: label.to_string(),
        unit: unit.to_string(),
        evidence: e,
    }
}

/// Wall time for a sample that only carries a monotonic `t` (seconds).
pub(crate) fn mono_wall(s: &SessionData, t: f64) -> u64 {
    s.wall_base_ms.saturating_add((t * 1000.0) as u64)
}

// ---------------------------------------------------------------------------
// Domain views
// ---------------------------------------------------------------------------

pub fn battery_view(s: &SessionData) -> Option<LiveBattery> {
    let b = s.battery.last()?;
    let extra = s.battery_extra.last();
    let ac = extra.map(|e| ev(&e.ac, "bool"));
    let state = match ac.as_ref().and_then(|a| a.value) {
        Some(v) if v >= 0.5 => {
            if b.charge.val().is_some() {
                "charging"
            } else {
                "ac-connected"
            }
        }
        Some(_) => {
            if b.discharge.val().is_some() {
                "discharging"
            } else {
                "idle"
            }
        }
        None => "unknown",
    };
    let meta = &s.battery_meta;
    let full = meta
        .full_mwh
        .map(|mwh| meta_full_ev("Wh", mwh / 1000.0, &meta.full_prov));
    let design = meta
        .design_mwh
        .map(|mwh| meta_full_ev("Wh", mwh / 1000.0, &meta.design_prov));
    Some(LiveBattery {
        state: state.to_string(),
        pct: Some(ev(&b.pct, "%")),
        discharge: Some(ev(&b.discharge, "W")),
        charge: Some(ev(&b.charge, "W")),
        remaining_wh: Some(ev(&b.remaining_wh, "Wh")),
        runtime_s: None,
        health_pct: extra.map(|e| ev(&e.health, "ratio")),
        full_charge_wh: full,
        design_wh: design,
        ac,
    })
}

fn meta_full_ev(unit: &str, value: f64, prov: &str) -> EvidenceValue {
    let provenance = match prov {
        "measured" | "derived" | "estimated" => prov,
        _ => "unavailable",
    };
    EvidenceValue {
        value: Some(value),
        provenance: provenance.to_string(),
        quality: "fresh".to_string(),
        absence_kind: None,
        reason: None,
        source: "battery".to_string(),
        collector: "battery".to_string(),
        unit: unit.to_string(),
        wall_ms: 0,
        mono_ms: 0,
        age_ms: None,
    }
}

pub fn cpu_view(s: &SessionData) -> Option<LiveCpu> {
    let c = s.cpu.last()?;
    Some(LiveCpu {
        utility: Some(ev(&c.utility, "%")),
        package_power: Some(ev(&c.pkg_power_w, "W")),
        package_derived: Some(ev(&c.pkg_derived_w, "W")),
        freq_mhz: None,
        c3_pct: Some(ev(&c.c3_pct, "%")),
        core_count: c.cores.len(),
    })
}

pub fn gpu_views(s: &SessionData) -> Vec<LiveGpuAdapter> {
    let Some(g) = s.gpu.last() else {
        return Vec::new();
    };
    g.adapters
        .iter()
        .map(|a| LiveGpuAdapter {
            name: a.name.clone(),
            discrete: a.discrete,
            utilization: Some(ev(&a.total_util, "%")),
            memory_mb: Some(ev(&a.mem_dedicated_mb, "MB")),
            power: Some(EvidenceValue::unavailable(
                "W",
                "unsupported",
                "no GPU power sensor on this adapter; vendor APIs pending (NVML/ADL)",
            )),
            awake: a
                .awake
                .as_ref()
                .map(|w| {
                    format!(
                        "{} ({})",
                        if w.active { "awake" } else { "quiet" },
                        w.confidence
                    )
                })
                .unwrap_or_else(|| "unknown".to_string()),
        })
        .collect()
}

pub fn display_view(s: &SessionData) -> Option<LiveDisplay> {
    let d = s.display.last()?;
    let displays = d
        .displays
        .iter()
        .map(|e| DisplayInfo {
            name: e.name.clone(),
            primary: e.primary,
            width: e.width,
            height: e.height,
            refresh_hz: e.freq_hz,
        })
        .collect();
    Some(LiveDisplay {
        count: d.displays.len(),
        brightness: Some(ev(&d.brightness, "%")),
        displays,
    })
}

pub fn system_view(s: &SessionData) -> LiveSystem {
    let net = s.net.last();
    let storage = s.storage.last();
    let extra = s.battery_extra.last();
    let foreground = s
        .procs
        .last()
        .and_then(|p| p.top.first())
        .map(|p| format!("{} (pid {})", p.name, p.pid));
    LiveSystem {
        ac: extra.map(|e| ev(&e.ac, "bool")),
        scheme: s.policy.scheme_name.clone(),
        foreground_process: foreground,
        process_count: s.procs.last().map(|p| p.top.len()).unwrap_or(0),
        net_rx: net.map(|n| ev(&n.total_rx, "B/s")),
        net_tx: net.map(|n| ev(&n.total_tx, "B/s")),
        storage_activity: storage.map(|x| ev(&x.disk_time, "%")),
        storage_read: storage.map(|x| ev(&x.read_bps, "B/s")),
        storage_write: storage.map(|x| ev(&x.write_bps, "B/s")),
    }
}

pub fn process_rows(s: &SessionData) -> Vec<ProcessRow> {
    s.procs
        .last()
        .map(|p| {
            p.top
                .iter()
                .map(|e| ProcessRow {
                    pid: e.pid,
                    name: e.name.clone(),
                    cpu_pct: Some(ev(&e.cpu, "%")),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Headline strip. Values with no evidence render as unavailable, never 0.
pub fn headline(s: &SessionData) -> Vec<MetricEvidence> {
    let b = s.battery.last();
    let system_power = b.map(|p| ev(&p.discharge, "W")).unwrap_or_else(|| {
        EvidenceValue::unavailable("W", "not-sampled", "no battery sample recorded")
    });
    let battery = b.map(|p| ev(&p.pct, "%")).unwrap_or_else(|| {
        EvidenceValue::unavailable("%", "not-sampled", "no battery sample recorded")
    });
    let cpu = s
        .cpu
        .last()
        .map(|p| ev(&p.utility, "%"))
        .unwrap_or_else(|| {
            EvidenceValue::unavailable("%", "not-sampled", "no cpu sample recorded")
        });
    let gpu = s
        .gpu
        .last()
        .and_then(|g| g.adapters.first().map(|a| ev(&a.total_util, "%")))
        .unwrap_or_else(|| {
            EvidenceValue::unavailable("%", "unsupported", "no GPU utilization sample")
        });
    let display = s
        .display
        .last()
        .map(|p| ev(&p.brightness, "%"))
        .unwrap_or_else(|| {
            EvidenceValue::unavailable("%", "unsupported", "no brightness sensor matched a display")
        });
    let storage = s
        .storage
        .last()
        .map(|p| ev(&p.disk_time, "%"))
        .unwrap_or_else(|| EvidenceValue::unavailable("%", "not-sampled", "no storage sample"));
    let network = s
        .net
        .last()
        .map(|p| ev(&p.total_rx, "B/s"))
        .unwrap_or_else(|| EvidenceValue::unavailable("B/s", "not-sampled", "no network sample"));

    vec![
        metric("system_power", "System power", "W", system_power),
        metric("battery", "Battery", "%", battery),
        metric("cpu", "CPU", "%", cpu),
        metric("gpu", "GPU", "%", gpu),
        metric("display", "Display", "%", display),
        metric("storage", "Storage", "%", storage),
        metric("network", "Network", "B/s", network),
    ]
}

/// Per-collector health for the Evidence panel. Freshness (samples, observed
/// Hz, failures) comes from the backend dashboard helper over the observed
/// window (the file tail); `observed` is the collector set discovered by the
/// derived index (`None` when the file was too large to scan, so absence is
/// never claimed without evidence). The state label adds only a staleness
/// threshold around the recorded cadence.
pub fn collectors_view(
    s: &SessionData,
    observed: Option<&[String]>,
    base_interval_ms: u64,
    caps: &CapabilityReport,
    reference_wall_ms: u64,
) -> Vec<CollectorState> {
    let now = if reference_wall_ms > 0 {
        reference_wall_ms
    } else {
        now_wall_ms()
    };
    let last_walls = [
        (
            "battery",
            s.battery.last().map(|p| p.discharge.stamp.wall_millis),
        ),
        ("cpu", s.cpu.last().map(|p| p.utility.stamp.wall_millis)),
        ("gpu", s.gpu.last().map(|p| mono_wall(s, p.t))),
        ("proc", s.procs.last().map(|p| mono_wall(s, p.t))),
        (
            "display",
            s.display.last().map(|p| p.brightness.stamp.wall_millis),
        ),
        ("net", s.net.last().map(|p| p.total_rx.stamp.wall_millis)),
        (
            "storage",
            s.storage.last().map(|p| p.disk_time.stamp.wall_millis),
        ),
        ("usb", s.usb.last().map(|p| mono_wall(s, p.t))),
        (
            "self",
            s.selfmon.last().map(|p| p.cpu_pct.stamp.wall_millis),
        ),
        (
            "os_power",
            s.policy_history.last().map(|(t, _)| mono_wall(s, *t)),
        ),
    ];
    let freshness = dashboard::freshness(s);
    let mut out = Vec::new();
    for (name, last) in last_walls {
        let f = freshness.iter().find(|f| f.name == name);
        let samples = f.map(|f| f.samples as u64).unwrap_or(0);
        let failures = f.map(|f| f.failures as u64).unwrap_or(0);
        let observed_hz = f.map(|f| f.observed_hz).filter(|h| *h > 0.0);
        let (admin, helps) = capability_admin(caps, name);
        let present = observed.map(|o| o.iter().any(|c| c == name));
        let (state, reason) = if present == Some(false) {
            (
                "unavailable",
                Some("not present in this recording".to_string()),
            )
        } else if samples == 0 {
            if present == Some(true) {
                (
                    "stale",
                    Some("not sampled in the observed window".to_string()),
                )
            } else {
                (
                    "unavailable",
                    Some("no samples in the observed window".to_string()),
                )
            }
        } else if failures >= samples {
            (
                "unavailable",
                Some("every sample in the window failed".to_string()),
            )
        } else if last.is_some_and(|w| now.saturating_sub(w) > stale_after_ms(base_interval_ms)) {
            (
                "stale",
                Some("last sample is older than the expected cadence".to_string()),
            )
        } else if failures > 0 {
            (
                "degraded",
                Some(format!("{failures} of {samples} samples unavailable")),
            )
        } else {
            ("available", None)
        };
        out.push(CollectorState {
            name: name.to_string(),
            state: state.to_string(),
            reason,
            last_sample_wall_ms: last.filter(|w| *w > 0),
            samples,
            failures,
            observed_hz,
            requires_admin: admin,
            elevation_would_help: helps,
        });
    }
    out
}

fn stale_after_ms(base_interval_ms: u64) -> u64 {
    base_interval_ms
        .saturating_mul(3)
        .saturating_add(2_000)
        .max(3_000)
}

fn capability_admin(caps: &CapabilityReport, name: &str) -> (bool, bool) {
    let Some(c) = caps.collectors.iter().find(|c| c.name == name) else {
        return (false, false);
    };
    let admin = c.fields.iter().any(|f| f.requires_admin);
    let helps = c.fields.iter().any(|f| f.elevation_would_help);
    (admin, helps)
}

pub fn events_view(s: &SessionData, limit: usize) -> Vec<TimelineEvent> {
    let start = s.events.len().saturating_sub(limit);
    s.events[start..]
        .iter()
        .map(|e| TimelineEvent {
            wall_ms: e.wall_ms,
            mono_ms: e.mono_ms,
            kind: e.kind.clone(),
            detail: e.detail.clone(),
            severity: severity_for(&e.kind).to_string(),
        })
        .collect()
}

fn severity_for(kind: &str) -> &'static str {
    let k = kind.to_ascii_lowercase();
    if k == "marker" {
        "marker"
    } else if k.contains("error") || k.contains("fail") {
        "error"
    } else if k.contains("timeout")
        || k.contains("stale")
        || k.contains("anomaly")
        || k.contains("disconnect")
        || k.contains("change")
    {
        "warning"
    } else {
        "info"
    }
}

/// One timeline point from a reading (value None when unavailable).
fn point(t_ms: u64, t: &Telemetry<f64>) -> TimelinePoint {
    TimelinePoint {
        t_ms,
        value: t.val(),
        provenance: t.provenance().as_str().to_string(),
        quality: t.quality.as_str().to_string(),
    }
}

/// Every chartable domain series, downsampled to `max_points`. Gaps are
/// preserved; no value is interpolated. `pid`, when set, adds that process's
/// top-list CPU series so a selected process can be inspected on the shared
/// timeline (absence is a gap, never zero).
pub fn domain_series(s: &SessionData, max_points: usize, pid: Option<u32>) -> Vec<Series> {
    let cpu = collect_points(s.cpu.len(), |i| {
        let p = &s.cpu[i];
        point(series_wall(&p.utility, s, p.t), &p.utility)
    });
    let cpu_pkg = collect_points(s.cpu.len(), |i| {
        let p = &s.cpu[i];
        point(series_wall(&p.pkg_derived_w, s, p.t), &p.pkg_derived_w)
    });
    let cpu_pkg_measured = collect_points(s.cpu.len(), |i| {
        let p = &s.cpu[i];
        point(series_wall(&p.pkg_power_w, s, p.t), &p.pkg_power_w)
    });
    let gpu = collect_points(s.gpu.len(), |i| {
        let p = &s.gpu[i];
        let t = p.adapters.first().map(|a| &a.total_util);
        match t {
            Some(t) => point(mono_wall(s, p.t), t),
            None => TimelinePoint {
                t_ms: mono_wall(s, p.t),
                value: None,
                provenance: "unavailable".to_string(),
                quality: "unknown".to_string(),
            },
        }
    });
    let display = collect_points(s.display.len(), |i| {
        let p = &s.display[i];
        point(series_wall(&p.brightness, s, p.t), &p.brightness)
    });
    let net = collect_points(s.net.len(), |i| {
        let p = &s.net[i];
        point(series_wall(&p.total_rx, s, p.t), &p.total_rx)
    });
    let net_tx = collect_points(s.net.len(), |i| {
        let p = &s.net[i];
        point(series_wall(&p.total_tx, s, p.t), &p.total_tx)
    });
    let storage = collect_points(s.storage.len(), |i| {
        let p = &s.storage[i];
        point(series_wall(&p.disk_time, s, p.t), &p.disk_time)
    });
    let storage_read = collect_points(s.storage.len(), |i| {
        let p = &s.storage[i];
        point(series_wall(&p.read_bps, s, p.t), &p.read_bps)
    });
    let storage_write = collect_points(s.storage.len(), |i| {
        let p = &s.storage[i];
        point(series_wall(&p.write_bps, s, p.t), &p.write_bps)
    });
    let battery_pct = collect_points(s.battery.len(), |i| {
        let p = &s.battery[i];
        point(series_wall(&p.pct, s, p.t), &p.pct)
    });
    let battery = collect_points(s.battery.len(), |i| {
        let p = &s.battery[i];
        point(series_wall(&p.discharge, s, p.t), &p.discharge)
    });
    let battery_charge = collect_points(s.battery.len(), |i| {
        let p = &s.battery[i];
        point(series_wall(&p.charge, s, p.t), &p.charge)
    });
    // Top-of-list CPU activity (the busiest recorded process each tick) is a
    // generic activity track; the per-pid series below is opt-in.
    let proc_top = collect_points(s.procs.len(), |i| {
        let p = &s.procs[i];
        let best = p
            .top
            .iter()
            .filter_map(|e| e.cpu.val().map(|v| (e, v)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((e, _)) => point(mono_wall(s, p.t), &e.cpu),
            None => TimelinePoint {
                t_ms: mono_wall(s, p.t),
                value: None,
                provenance: "unavailable".to_string(),
                quality: "unknown".to_string(),
            },
        }
    });

    let mut out = vec![
        to_series(
            "battery_discharge_w",
            "Battery discharge",
            "W",
            battery,
            max_points,
        ),
        to_series(
            "battery_charge_w",
            "Battery charge",
            "W",
            battery_charge,
            max_points,
        ),
        to_series(
            "battery_pct",
            "Battery charge",
            "%",
            battery_pct,
            max_points,
        ),
        to_series("cpu_utility_pct", "CPU utility", "%", cpu, max_points),
        to_series(
            "cpu_pkg_derived_w",
            "CPU package (derived)",
            "W",
            cpu_pkg,
            max_points,
        ),
        to_series(
            "cpu_pkg_power_w",
            "CPU package (measured)",
            "W",
            cpu_pkg_measured,
            max_points,
        ),
        to_series("gpu_util_pct", "GPU utilization", "%", gpu, max_points),
        to_series(
            "display_brightness_pct",
            "Display brightness",
            "%",
            display,
            max_points,
        ),
        to_series("net_rx_bps", "Network RX", "B/s", net, max_points),
        to_series("net_tx_bps", "Network TX", "B/s", net_tx, max_points),
        to_series(
            "storage_disk_time_pct",
            "Disk activity",
            "%",
            storage,
            max_points,
        ),
        to_series(
            "storage_read_bps",
            "Disk read",
            "B/s",
            storage_read,
            max_points,
        ),
        to_series(
            "storage_write_bps",
            "Disk write",
            "B/s",
            storage_write,
            max_points,
        ),
        to_series(
            "proc_top_cpu_pct",
            "Busiest process CPU",
            "%",
            proc_top,
            max_points,
        ),
    ];
    if let Some(pid) = pid {
        let series = collect_points(s.procs.len(), |i| {
            let p = &s.procs[i];
            match p.top.iter().find(|e| e.pid == pid) {
                Some(e) => point(mono_wall(s, p.t), &e.cpu),
                None => TimelinePoint {
                    t_ms: mono_wall(s, p.t),
                    value: None,
                    provenance: "unavailable".to_string(),
                    quality: "unknown".to_string(),
                },
            }
        });
        out.push(to_series(
            &format!("proc_pid_{pid}_cpu_pct"),
            &format!("Process {pid} CPU"),
            "%",
            series,
            max_points,
        ));
    }
    out
}

fn collect_points<F>(len: usize, mut f: F) -> Vec<TimelinePoint>
where
    F: FnMut(usize) -> TimelinePoint,
{
    (0..len).map(&mut f).collect()
}

fn to_series(key: &str, label: &str, unit: &str, points: Vec<TimelinePoint>, max: usize) -> Series {
    let (points, _down) = downsample(points, max);
    Series {
        key: key.to_string(),
        label: label.to_string(),
        unit: unit.to_string(),
        points,
    }
}

pub fn series_wall(t: &Telemetry<f64>, s: &SessionData, mono_s: f64) -> u64 {
    if t.stamp.wall_millis > 0 {
        t.stamp.wall_millis
    } else {
        mono_wall(s, mono_s)
    }
}

/// Downsample to at most `max` points while preserving what matters for a
/// power timeline: per-bucket minimum and maximum (so short spikes are never
/// averaged away), the bucket's first/last for continuity, and one gap marker
/// when a bucket contains unavailable readings. Gaps stay gaps.
pub fn downsample(points: Vec<TimelinePoint>, max: usize) -> (Vec<TimelinePoint>, bool) {
    if max == 0 || points.len() <= max {
        return (points, false);
    }
    let max = max.max(4);
    let buckets = (max / 4).max(1);
    let size = points.len().div_ceil(buckets);
    let mut out: Vec<TimelinePoint> = Vec::with_capacity(max);
    for chunk in points.chunks(size) {
        let mut min: Option<usize> = None;
        let mut max_i: Option<usize> = None;
        let mut gap: Option<usize> = None;
        // A provenance/quality transition is semantically meaningful even when
        // the numeric value barely moves; never let it vanish in a bucket.
        let mut transition: Option<usize> = None;
        let first = &chunk[0];
        for (i, p) in chunk.iter().enumerate() {
            match p.value {
                None => {
                    if gap.is_none() {
                        gap = Some(i);
                    }
                }
                Some(v) => {
                    if min.is_none_or(|j| chunk[j].value.unwrap_or(f64::MAX) > v) {
                        min = Some(i);
                    }
                    if max_i.is_none_or(|j| chunk[j].value.unwrap_or(f64::MIN) < v) {
                        max_i = Some(i);
                    }
                }
            }
            if transition.is_none()
                && i > 0
                && (p.provenance != first.provenance || p.quality != first.quality)
            {
                transition = Some(i);
            }
        }
        let mut picks: Vec<usize> = Vec::with_capacity(6);
        picks.push(0);
        if let Some(i) = min {
            picks.push(i);
        }
        if let Some(i) = max_i {
            picks.push(i);
        }
        if let Some(i) = gap {
            picks.push(i);
        }
        if let Some(i) = transition {
            picks.push(i);
        }
        picks.push(chunk.len() - 1);
        picks.sort_unstable();
        picks.dedup();
        for i in picks {
            out.push(chunk[i].clone());
        }
    }
    (out, true)
}

/// Age every evidence reading in a recorded session summary relative to the
/// session's end time. `None` stays `None`; a reading is never given a
/// fabricated age.
pub fn stamp_summary_ages(summary: &mut crate::dto::SessionSummary, end_wall_ms: u64) {
    fn one(e: &mut EvidenceValue, now: u64) {
        e.age_ms = if e.wall_ms > 0 && now >= e.wall_ms {
            Some(now - e.wall_ms)
        } else {
            None
        };
    }
    fn opt(e: &mut Option<EvidenceValue>, now: u64) {
        if let Some(v) = e.as_mut() {
            one(v, now);
        }
    }
    if let Some(b) = summary.battery.as_mut() {
        for f in [
            &mut b.pct,
            &mut b.discharge,
            &mut b.charge,
            &mut b.remaining_wh,
            &mut b.runtime_s,
            &mut b.health_pct,
            &mut b.full_charge_wh,
            &mut b.design_wh,
            &mut b.ac,
        ] {
            opt(f, end_wall_ms);
        }
    }
    if let Some(c) = summary.cpu.as_mut() {
        for f in [&mut c.utility, &mut c.package_power, &mut c.package_derived] {
            opt(f, end_wall_ms);
        }
    }
    for g in &mut summary.gpus {
        for f in [&mut g.utilization, &mut g.memory_mb, &mut g.power] {
            opt(f, end_wall_ms);
        }
    }
    if let Some(d) = summary.display.as_mut() {
        opt(&mut d.brightness, end_wall_ms);
    }
    for f in [
        &mut summary.system.ac,
        &mut summary.system.net_rx,
        &mut summary.system.net_tx,
        &mut summary.system.storage_activity,
        &mut summary.system.storage_read,
        &mut summary.system.storage_write,
    ] {
        opt(f, end_wall_ms);
    }
    for p in &mut summary.processes {
        opt(&mut p.cpu_pct, end_wall_ms);
    }
}

pub fn footer_summary(v: Option<&JVal>) -> FooterSummary {
    let Some(v) = v else {
        return FooterSummary::default();
    };
    let n = |k: &str| v.get(k).and_then(|x| x.num());
    let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    FooterSummary {
        samples: n("samples"),
        discharge_wh: n("discharge_wh"),
        charge_wh: n("charge_wh"),
        discharge_median_w: n("discharge_median_w"),
        discharge_coverage_pct: n("discharge_coverage_pct"),
        discharge_unknown_s: n("discharge_unknown_s"),
        discharge_unobserved_s: n("discharge_unobserved_s"),
        discharge_discontinuities: n("discharge_discontinuities"),
        cpu_utility_median_pct: n("cpu_utility_median_pct"),
        cpu_pkg_derived_median_w: n("cpu_pkg_derived_median_w"),
        collector_timeouts: n("collector_timeouts"),
        session_bytes: n("session_bytes"),
        recovered: b("recovered"),
    }
}

/// Set `age_ms` on every evidence reading in a live snapshot relative to the
/// view's `generated_at`. Independently sampled readings therefore carry
/// distinct ages; they are never presented as one simultaneous measurement.
/// Readings without a wall timestamp keep `None` (age unknown, not zero).
pub fn stamp_ages(snap: &mut LiveSnapshot, generated_at: u64) {
    fn one(e: &mut EvidenceValue, now: u64) {
        e.age_ms = if e.wall_ms > 0 && now >= e.wall_ms {
            Some(now - e.wall_ms)
        } else {
            None
        };
    }
    fn opt(e: &mut Option<EvidenceValue>, now: u64) {
        if let Some(v) = e.as_mut() {
            one(v, now);
        }
    }
    for m in &mut snap.headline {
        one(&mut m.evidence, generated_at);
    }
    if let Some(b) = snap.battery.as_mut() {
        for f in [
            &mut b.pct,
            &mut b.discharge,
            &mut b.charge,
            &mut b.remaining_wh,
            &mut b.runtime_s,
            &mut b.health_pct,
            &mut b.full_charge_wh,
            &mut b.design_wh,
            &mut b.ac,
        ] {
            opt(f, generated_at);
        }
    }
    if let Some(c) = snap.cpu.as_mut() {
        for f in [
            &mut c.utility,
            &mut c.package_power,
            &mut c.package_derived,
            &mut c.freq_mhz,
            &mut c.c3_pct,
        ] {
            opt(f, generated_at);
        }
    }
    for g in &mut snap.gpus {
        for f in [&mut g.utilization, &mut g.memory_mb, &mut g.power] {
            opt(f, generated_at);
        }
    }
    if let Some(d) = snap.display.as_mut() {
        opt(&mut d.brightness, generated_at);
    }
    for f in [
        &mut snap.system.ac,
        &mut snap.system.net_rx,
        &mut snap.system.net_tx,
        &mut snap.system.storage_activity,
        &mut snap.system.storage_read,
        &mut snap.system.storage_write,
    ] {
        opt(f, generated_at);
    }
}
