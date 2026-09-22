//! Compact columnar session archive: manifest.json + samples.bin +
//! events.jsonl. Each sample is a 19-byte record (mono_ms u64, metric u16,
//! provenance u8, value f64) — roughly 8x smaller than JSONL — with a
//! metric table in the manifest so files stay self-describing.
//!
//! Static metric IDs cover scalar series; GPU adapters and process PIDs
//! get dynamic IDs (>= 1000) recorded in the manifest with their names.
//! Session policy is NOT archived (re-read from JSONL when needed).
//!
//! Compatibility & product intent (explicit):
//! * `pf-archive-1` (v1) is a SUPPORTED read contract, pinned by a real,
//!   hand-encoded fixture (`real_v1_fixture_loads_with_expected_evidence`).
//!   v1 lacks provenance quality/kind, wall time, and adapter metadata; the
//!   reader demotes unknown provenance to Unavailable rather than Measured.
//! * `pf-archive-2` (v2) is the current writer and full-fidelity reader.
//! * Archives are produced by the `store` CLI verb. There is intentionally
//!   no CLI reader: `load_archive_v2` (and `load_archive` for v1) are the
//!   supported programmatic read path. A CLI read verb would be added only
//!   if product requirements call for user-facing archive browsing.
//! * `source_hash` is a weak identity fingerprint, NOT integrity evidence:
//!   it covers label/record-count only and is never used to accept/reject.

use crate::json::{JVal, parse, to_json};
use crate::session::SessionData;
use crate::telemetry::{ClockStamp, Reading, SampleQuality, Telemetry, UnavailKind};
use std::collections::HashMap;

/// (collector, metric, unit) for static IDs 0..=49. The last 14 are the
/// optional per-sample monotonic acquisition windows (see the timing
/// constants below); they were appended so older v2/v1 files stay readable.
const STATIC_METRICS: &[(&str, &str, &str)] = &[
    ("battery", "discharge_w", "W"),
    ("battery", "charge_w", "W"),
    ("battery", "remaining_wh", "Wh"),
    ("battery", "charge_pct", "%"),
    ("cpu", "utility_pct", "%"),
    ("cpu", "c3_pct", "%"),
    ("cpu", "idle_breaks_s", "/s"),
    ("cpu", "pkg_derived_w", "W"),
    ("display", "brightness_pct", "%"),
    ("net", "total_rx_bps", "B/s"),
    ("net", "total_tx_bps", "B/s"),
    ("storage", "disk_time_pct", "%"),
    ("storage", "read_bps", "B/s"),
    ("storage", "write_bps", "B/s"),
    ("usb", "count.usb", "n"),
    ("usb", "count.bluetooth", "n"),
    ("usb", "count.camera", "n"),
    ("usb", "count.audio", "n"),
    ("usb", "count.hid", "n"),
    ("self", "cpu_pct", "%"),
    ("self", "ws_mb", "MB"),
    ("self", "io_write_b", "B"),
    ("self", "threads", "n"),
    ("battery", "ac", "bool"),
    ("battery", "rate_mw", "mW"),
    ("battery", "health", "ratio"),
    ("battery", "temperature_raw", "raw"),
    ("battery", "t_start_ms", "ms"),
    ("battery", "t_end_ms", "ms"),
    ("cpu", "pkg_power_w", "W"),
    ("cpu", "ctx_switches", "/s"),
    ("cpu", "t_start_ms", "ms"),
    ("cpu", "t_end_ms", "ms"),
    ("self", "priv_mb", "MB"),
    ("self", "io_read_b", "B"),
    ("self", "ctx_switches_s", "ctx/s"),
    // Optional per-sample acquisition windows (monotonic ms). A record is
    // written only when the collector reported one, so absence reloads as
    // None instead of a fabricated 0.
    ("gpu", "t_start_ms", "ms"),
    ("gpu", "t_end_ms", "ms"),
    ("proc", "t_start_ms", "ms"),
    ("proc", "t_end_ms", "ms"),
    ("display", "t_start_ms", "ms"),
    ("display", "t_end_ms", "ms"),
    ("net", "t_start_ms", "ms"),
    ("net", "t_end_ms", "ms"),
    ("storage", "t_start_ms", "ms"),
    ("storage", "t_end_ms", "ms"),
    ("usb", "t_start_ms", "ms"),
    ("usb", "t_end_ms", "ms"),
    ("self", "t_start_ms", "ms"),
    ("self", "t_end_ms", "ms"),
];

/// Static ids (indices into STATIC_METRICS) referenced by name, so the
/// migrator/loader cannot drift from the table above.
const ID_BATTERY_AC: u16 = 23;
const ID_BATTERY_RATE_MW: u16 = 24;
const ID_BATTERY_HEALTH: u16 = 25;
const ID_BATTERY_TEMP_RAW: u16 = 26;
const ID_BATTERY_T_START: u16 = 27;
const ID_BATTERY_T_END: u16 = 28;
const ID_CPU_PKG_POWER: u16 = 29;
const ID_CPU_CTX_SWITCHES: u16 = 30;
const ID_CPU_T_START: u16 = 31;
const ID_CPU_T_END: u16 = 32;
const ID_SELF_PRIV_MB: u16 = 33;
const ID_SELF_IO_READ: u16 = 34;
const ID_SELF_CTX_SWITCHES: u16 = 35;
// Optional acquisition windows for the remaining series (battery/cpu use 27,
// 28, 31, 32 above). Appended after the historical static range.
const ID_GPU_T_START: u16 = 36;
const ID_GPU_T_END: u16 = 37;
const ID_PROC_T_START: u16 = 38;
const ID_PROC_T_END: u16 = 39;
const ID_DISPLAY_T_START: u16 = 40;
const ID_DISPLAY_T_END: u16 = 41;
const ID_NET_T_START: u16 = 42;
const ID_NET_T_END: u16 = 43;
const ID_STORAGE_T_START: u16 = 44;
const ID_STORAGE_T_END: u16 = 45;
const ID_USB_T_START: u16 = 46;
const ID_USB_T_END: u16 = 47;
const ID_SELF_T_START: u16 = 48;
const ID_SELF_T_END: u16 = 49;

const DYNAMIC_BASE: u16 = 1000;
pub const RECORD_LEN: usize = 19;

/// Public view of the static metric table for schema-completeness checks.
pub const STATIC_METRICS_PUB: &[(&str, &str, &str)] = STATIC_METRICS;

/// Legacy v1 provenance codec. Unknown strings/codes map to
/// Unavailable (3/"unavailable") — never to Measured.
pub fn prov_byte(p: &str) -> u8 {
    match p {
        "measured" => 0,
        "derived" => 1,
        "estimated" => 2,
        "unavailable" => 3,
        _ => 3,
    }
}

pub fn prov_str(b: u8) -> &'static str {
    match b {
        0 => "measured",
        1 => "derived",
        2 => "estimated",
        _ => "unavailable",
    }
}

fn esc(s: &str) -> String {
    to_json(&JVal::Str(s.to_string()))[1..]
        .trim_end_matches('"')
        .to_string()
}

#[derive(Debug, Clone)]
struct DynEntry {
    id: u16,
    collector: String,
    metric: String,
    unit: String,
    extra: String,
}

/// Register (or reuse) a dynamic metric id keyed by (collector, metric,
/// extra). First-seen order is deterministic and preserves interface order.
fn register(
    dyn_table: &mut Vec<DynEntry>,
    next: &mut u16,
    collector: &str,
    metric: &str,
    unit: &str,
    extra: &str,
) -> u16 {
    if let Some(d) = dyn_table
        .iter()
        .find(|d| d.collector == collector && d.metric == metric && d.extra == extra)
    {
        return d.id;
    }
    let id = *next;
    dyn_table.push(DynEntry {
        id,
        collector: collector.to_string(),
        metric: metric.to_string(),
        unit: unit.to_string(),
        extra: extra.to_string(),
    });
    *next += 1;
    id
}

pub struct Archive {
    pub manifest: String,
    pub samples: Vec<u8>,
    pub events: Vec<String>,
    pub records: usize,
}

fn get_u64(buf: &[u8], off: usize) -> Option<u64> {
    let s = buf.get(off..off + 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(u64::from_le_bytes(a))
}

fn get_u16(buf: &[u8], off: usize) -> Option<u16> {
    let s = buf.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn get_f64(buf: &[u8], off: usize) -> Option<f64> {
    get_u64(buf, off).map(f64::from_bits)
}

/// Archive v2 record layout (all little-endian, explicit):
/// mono u64 | wall u64 | metric u16 | prov u8 | kind u8 | quality u8 |
/// flags u8 (bit0 = has value) | value f64 | reason_len u16 | reason bytes.
/// Fixed 33-byte header; reason length 0 when present.
pub const V2_HEADER_LEN: usize = 32;
pub const V2_FORMAT: &str = "pf-archive-2";
pub const V2_SCHEMA: u32 = 1;
/// Legacy format string for `load_archive`. v1 remains a supported read
/// contract (see the module header).
pub const V1_FORMAT: &str = "pf-archive-1";

#[allow(clippy::too_many_arguments)]
fn put_rec(
    buf: &mut Vec<u8>,
    mono_ms: u64,
    wall_ms: u64,
    metric: u16,
    prov: u8,
    kind: u8,
    quality: u8,
    val: Option<f64>,
    reason: &str,
) {
    buf.extend_from_slice(&mono_ms.to_le_bytes());
    buf.extend_from_slice(&wall_ms.to_le_bytes());
    buf.extend_from_slice(&metric.to_le_bytes());
    buf.push(prov);
    buf.push(kind);
    buf.push(quality);
    match val {
        Some(x) => {
            buf.push(1);
            buf.extend_from_slice(&x.to_le_bytes());
        }
        None => {
            buf.push(0);
            buf.extend_from_slice(&0f64.to_le_bytes());
        }
    }
    let rb = reason.as_bytes();
    let len = rb.len().min(1024) as u16;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&rb[..len as usize]);
}

/// Parse one v2 record; returns (mono, wall, metric, prov, kind, quality,
/// value-or-None, reason, next_offset). Unknown prov codes are reported
/// here (caller demotes, never fabricates).
#[allow(clippy::type_complexity)]
fn get_rec(
    buf: &[u8],
    off: usize,
) -> Option<(u64, u64, u16, u8, u8, u8, Option<f64>, String, usize)> {
    if buf.len() < off + V2_HEADER_LEN {
        return None;
    }
    let mono = u64::from_le_bytes(buf[off..off + 8].try_into().ok()?);
    let wall = u64::from_le_bytes(buf[off + 8..off + 16].try_into().ok()?);
    let metric = u16::from_le_bytes(buf[off + 16..off + 18].try_into().ok()?);
    let (prov, kind, quality, flags) = (buf[off + 18], buf[off + 19], buf[off + 20], buf[off + 21]);
    let value = f64::from_le_bytes(buf[off + 22..off + 30].try_into().ok()?);
    let rlen = u16::from_le_bytes(buf[off + 30..off + 32].try_into().ok()?) as usize;
    let end = off + V2_HEADER_LEN + rlen;
    if buf.len() < end {
        return None;
    }
    let reason = String::from_utf8_lossy(&buf[off + V2_HEADER_LEN..end]).to_string();
    let val = if flags & 1 != 0 { Some(value) } else { None };
    Some((mono, wall, metric, prov, kind, quality, val, reason, end))
}

/// Best-effort machine identity for the archive manifest. Never fails:
/// hostname env vars or "unknown".
fn machine_id() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME", "HOST"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    "unknown".to_string()
}

/// Weak non-cryptographic identity fingerprint: u64 (DefaultHasher) over
/// label + wall_base_ms + record count, hex-encoded. It is NOT a
/// tamper-evident integrity digest and does not cover the sample bytes; it
/// only detects accidental identity-field drift. Recomputed on load when
/// present and compared, but mismatches are tolerated (never a hard failure).
fn source_hash_hex(label: &str, wall_base_ms: u64, records: usize) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    label.hash(&mut h);
    wall_base_ms.hash(&mut h);
    records.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Migrate a loaded session into archive bytes (pure; FS wrappers in main).
pub fn migrate(label: &str, source: &str, s: &SessionData) -> Archive {
    let mut dyn_table: Vec<DynEntry> = Vec::new();
    let mut dyn_next: u16 = DYNAMIC_BASE;
    let measured = |v: f64| Telemetry::<f64>::measured(v, "", ClockStamp::default());

    // Pre-register every dynamic metric in first-seen order (deterministic).
    // GPU adapters carry their discrete flag in a manifest table (v2).
    let mut gpu_names: Vec<(String, bool)> = Vec::new();
    for g in &s.gpu {
        for a in &g.adapters {
            if !gpu_names.iter().any(|(n, _)| n == &a.name) {
                gpu_names.push((a.name.clone(), a.discrete));
            }
        }
    }
    for (name, _) in &gpu_names {
        for (metric, unit) in [
            ("total_util_pct", "engine-%"),
            ("mem_dedicated_mb", "MB"),
            ("mem_shared_mb", "MB"),
            ("util_3d", "engine-%"),
            ("util_compute", "engine-%"),
            ("util_decode", "engine-%"),
            ("util_codec", "engine-%"),
            ("util_copy", "engine-%"),
            ("util_other", "engine-%"),
            ("engines_active", "n"),
            ("engines_seen", "n"),
            ("awake", "bool"),
        ] {
            register(&mut dyn_table, &mut dyn_next, "gpu", metric, unit, name);
        }
    }
    // Session-level staleness flag (one series, not per adapter).
    register(&mut dyn_table, &mut dyn_next, "gpu", "stale", "bool", "");
    // Network counter-set topology staleness (one series per session).
    register(
        &mut dyn_table,
        &mut dyn_next,
        "net",
        "topology_stale",
        "bool",
        "",
    );
    for g in &s.gpu {
        for a in &g.adapters {
            for (pid, _) in &a.top_pids {
                register(
                    &mut dyn_table,
                    &mut dyn_next,
                    "gpu",
                    "top_pid_util_pct",
                    "%",
                    &format!("{}|{pid}", a.name),
                );
            }
        }
    }
    // Process identity (pid/name/ppid/start/exit) goes to a manifest
    // table (v2); the metric entry keeps only the pid key.
    #[allow(clippy::type_complexity)]
    let mut proc_pids: Vec<(u32, String, u32, Option<u64>, Option<u64>)> = Vec::new();
    // PIDs that ever reported an exit: the timestamp is archived per tick,
    // while the identity table keeps the first-seen value.
    let mut proc_exit_names: Vec<String> = Vec::new();
    for p in &s.procs {
        for e in &p.top {
            let key = format!("{}:{}", e.pid, e.name);
            if e.exit_unix_ms.is_some() && !proc_exit_names.contains(&key) {
                proc_exit_names.push(key);
            }
            if !proc_pids.iter().any(|(pid, _, _, _, _)| *pid == e.pid) {
                proc_pids.push((
                    e.pid,
                    e.name.clone(),
                    e.ppid,
                    e.start_unix_ms,
                    e.exit_unix_ms,
                ));
            }
        }
    }
    for (pid, name, _, _, _) in &proc_pids {
        register(
            &mut dyn_table,
            &mut dyn_next,
            "proc",
            "cpu_pct",
            "%",
            &format!("{pid}:{name}"),
        );
    }
    for key in &proc_exit_names {
        register(
            &mut dyn_table,
            &mut dyn_next,
            "proc",
            "exit_unix_ms",
            "ms",
            key,
        );
    }
    // Process-observation coverage (one series each, not per-PID): thread
    // total, inaccessible-process count, and top-N truncation flag. Absence
    // stays absent so a legacy archive reloads as unknown, never complete.
    for (metric, unit) in [
        ("coverage.total_threads", "n"),
        ("coverage.inaccessible", "n"),
        ("coverage.truncated", "bool"),
    ] {
        register(&mut dyn_table, &mut dyn_next, "proc", metric, unit, "");
    }
    // Per-core CPU detail and every remaining total counter.
    for c in &s.cpu {
        for core in &c.cores {
            for (metric, unit) in [
                ("core.utility", "%"),
                ("core.performance", "%"),
                ("core.freq_mhz", "MHz"),
                ("core.parking", "raw"),
            ] {
                register(
                    &mut dyn_table,
                    &mut dyn_next,
                    "cpu",
                    metric,
                    unit,
                    &core.instance,
                );
            }
        }
        for (k, _) in &c.totals {
            register(
                &mut dyn_table,
                &mut dyn_next,
                "cpu",
                &format!("total.{k}"),
                "?",
                "",
            );
        }
    }
    // Every display, brightness sensor, and HDR target.
    for d in &s.display {
        for e in &d.displays {
            for (metric, unit) in [
                ("brightness_pct", "%"),
                ("width", "px"),
                ("height", "px"),
                ("freq_hz", "Hz"),
                ("bpp", "bit"),
                ("primary", "bool"),
            ] {
                register(
                    &mut dyn_table,
                    &mut dyn_next,
                    "display",
                    metric,
                    unit,
                    &e.name,
                );
            }
        }
        for (inst, _) in &d.unmatched_sensors {
            register(
                &mut dyn_table,
                &mut dyn_next,
                "display",
                "unmatched_brightness",
                "%",
                inst,
            );
        }
        for h in &d.hdr_targets {
            let extra = format!("{}|{}", h.adapter, h.target);
            register(
                &mut dyn_table,
                &mut dyn_next,
                "display",
                "hdr_supported",
                "bool",
                &extra,
            );
            register(
                &mut dyn_table,
                &mut dyn_next,
                "display",
                "hdr_enabled",
                "bool",
                &extra,
            );
        }
    }
    // Interface identity needed to interpret throughput, plus Wi-Fi state.
    for n in &s.net {
        for (inst, _, _) in &n.throughput {
            register(&mut dyn_table, &mut dyn_next, "net", "rx_bps", "B/s", inst);
            register(&mut dyn_table, &mut dyn_next, "net", "tx_bps", "B/s", inst);
        }
        for a in &n.adapters {
            for (metric, unit) in [
                ("adapter.tx_mbps", "Mbps"),
                ("adapter.rx_mbps", "Mbps"),
                ("adapter.in_octets", "B"),
                ("adapter.out_octets", "B"),
                ("adapter.oper", "state"),
                ("adapter.media", "state"),
                ("adapter.admin_up", "bool"),
            ] {
                register(&mut dyn_table, &mut dyn_next, "net", metric, unit, &a.alias);
            }
        }
        for w in &n.wifi {
            register(
                &mut dyn_table,
                &mut dyn_next,
                "net",
                "wifi.signal_pct",
                "%",
                &w.descr,
            );
            register(
                &mut dyn_table,
                &mut dyn_next,
                "net",
                "wifi.state",
                "state",
                &w.descr,
            );
        }
    }
    // Dynamic USB classes (unknown classes are never dropped) and power
    // requirement evidence.
    for u in &s.usb {
        for c in &u.classes {
            if !matches!(
                c.class.as_str(),
                "usb" | "bluetooth" | "camera" | "audio" | "hid"
            ) {
                register(&mut dyn_table, &mut dyn_next, "usb", "count", "n", &c.class);
            }
        }
        for p in &u.power_devices {
            register(
                &mut dyn_table,
                &mut dyn_next,
                "usb",
                "power_device",
                "bool",
                p,
            );
        }
    }
    let id_of = |collector: &str, metric: &str, extra: &str| {
        dyn_table
            .iter()
            .find(|d| d.collector == collector && d.metric == metric && d.extra == extra)
            .map(|d| d.id)
    };

    let mut samples: Vec<u8> = Vec::new();
    // Timing records go to a side buffer (a second closure cannot borrow the
    // same Vec as `rec`); they are concatenated below. Order is irrelevant
    // because the loader groups by (mono, metric id).
    let mut samples_t: Vec<u8> = Vec::new();
    let mut records = 0usize;
    let mut t_records = 0usize;
    let wbase = s.wall_base_ms;
    // v2 stores present AND absent values (absence carries kind/reason).
    // Wall time derives from the session base (loader-captured).
    let mut rec = |mono_s: f64, id: u16, v: &Telemetry<f64>| {
        let mono = (mono_s * 1000.0) as u64;
        let (prov, val, reason) = match &v.value {
            Reading::Measured(x) => (0u8, Some(*x), ""),
            Reading::Derived(x) => (1u8, Some(*x), ""),
            Reading::Estimated(x) => (2u8, Some(*x), ""),
            Reading::Unavailable(r) => (3u8, None, *r),
        };
        let kind = v.unavail_kind.unwrap_or(UnavailKind::NotSampled) as u8;
        put_rec(
            &mut samples,
            mono,
            wbase + mono,
            id,
            prov,
            kind,
            v.quality as u8,
            val,
            reason,
        );
        records += 1;
    };
    // Optional timing: write a record ONLY when the collector reported it.
    // No record means None on reload, so absence is never a measured 0.
    let mut t_rec = |mono_s: f64, id: u16, t: Option<u64>| {
        if let Some(v) = t {
            let mono = (mono_s * 1000.0) as u64;
            put_rec(
                &mut samples_t,
                mono,
                wbase + mono,
                id,
                0,
                UnavailKind::NotSampled as u8,
                SampleQuality::Fresh as u8,
                Some(v as f64),
                "",
            );
            t_records += 1;
        }
    };
    for (i, b) in s.battery.iter().enumerate() {
        rec(b.t, 0, &b.discharge);
        rec(b.t, 1, &b.charge);
        rec(b.t, 2, &b.remaining_wh);
        rec(b.t, 3, &b.pct);
        if let Some(x) = s.battery_extra.get(i) {
            rec(x.t, ID_BATTERY_AC, &x.ac);
            rec(x.t, ID_BATTERY_RATE_MW, &x.rate_mw);
            rec(x.t, ID_BATTERY_HEALTH, &x.health);
            rec(x.t, ID_BATTERY_TEMP_RAW, &x.temperature_raw);
            t_rec(x.t, ID_BATTERY_T_START, x.t_start_ms);
            t_rec(x.t, ID_BATTERY_T_END, x.t_end_ms);
        }
    }
    for c in &s.cpu {
        rec(c.t, 4, &c.utility);
        rec(c.t, 5, &c.c3_pct);
        rec(c.t, 6, &c.idle_breaks);
        rec(c.t, 7, &c.pkg_derived_w);
        rec(c.t, ID_CPU_PKG_POWER, &c.pkg_power_w);
        rec(c.t, ID_CPU_CTX_SWITCHES, &c.ctx_switches);
        t_rec(c.t, ID_CPU_T_START, c.t_start_ms);
        t_rec(c.t, ID_CPU_T_END, c.t_end_ms);
        for core in &c.cores {
            for (metric, v) in [
                ("core.utility", &core.utility),
                ("core.performance", &core.performance),
                ("core.freq_mhz", &core.freq_mhz),
                ("core.parking", &core.parking),
            ] {
                if let Some(id) = id_of("cpu", metric, &core.instance) {
                    rec(c.t, id, v);
                }
            }
        }
        for (k, v) in &c.totals {
            if let Some(id) = id_of("cpu", &format!("total.{k}"), "") {
                rec(c.t, id, v);
            }
        }
    }
    for g in &s.gpu {
        if let Some(id) = id_of("gpu", "stale", "") {
            rec(g.t, id, &measured(if g.stale { 1.0 } else { 0.0 }));
        }
        t_rec(g.t, ID_GPU_T_START, g.t_start_ms);
        t_rec(g.t, ID_GPU_T_END, g.t_end_ms);
        for a in &g.adapters {
            for (metric, v) in [
                ("total_util_pct", &a.total_util),
                ("mem_dedicated_mb", &a.mem_dedicated_mb),
                ("mem_shared_mb", &a.mem_shared_mb),
                ("util_3d", &a.util_3d),
                ("util_compute", &a.util_compute),
                ("util_decode", &a.util_decode),
                ("util_codec", &a.util_codec),
                ("util_copy", &a.util_copy),
                ("util_other", &a.util_other),
                ("engines_active", &a.engines_active),
                ("engines_seen", &a.engines_seen),
            ] {
                if let Some(id) = id_of("gpu", metric, &a.name) {
                    rec(g.t, id, v);
                }
            }
            if let (Some(aw), Some(id)) = (&a.awake, id_of("gpu", "awake", &a.name)) {
                rec(g.t, id, &measured(if aw.active { 1.0 } else { 0.0 }));
            }
            for (pid, v) in &a.top_pids {
                if let Some(id) = id_of("gpu", "top_pid_util_pct", &format!("{}|{pid}", a.name)) {
                    rec(g.t, id, v);
                }
            }
        }
    }
    for p in &s.procs {
        t_rec(p.t, ID_PROC_T_START, p.t_start_ms);
        t_rec(p.t, ID_PROC_T_END, p.t_end_ms);
        // Coverage metadata: written only when the collector reported it, so
        // absence reloads as None (unknown), never a fabricated complete tick.
        for (metric, val) in [
            ("coverage.total_threads", p.total_threads.map(|v| v as f64)),
            ("coverage.inaccessible", p.inaccessible.map(|v| v as f64)),
            (
                "coverage.truncated",
                p.truncated.map(|v| if v { 1.0 } else { 0.0 }),
            ),
        ] {
            if let (Some(v), Some(id)) = (val, id_of("proc", metric, "")) {
                rec(p.t, id, &measured(v));
            }
        }
        for e in &p.top {
            if let Some(id) = id_of("proc", "cpu_pct", &format!("{}:{}", e.pid, e.name)) {
                rec(p.t, id, &e.cpu);
            }
            if let (Some(exit), Some(id)) = (
                e.exit_unix_ms,
                id_of("proc", "exit_unix_ms", &format!("{}:{}", e.pid, e.name)),
            ) {
                rec(p.t, id, &measured(exit as f64));
            }
        }
    }
    for d in &s.display {
        t_rec(d.t, ID_DISPLAY_T_START, d.t_start_ms);
        t_rec(d.t, ID_DISPLAY_T_END, d.t_end_ms);
        // Static id 8 keeps older v2 archives (and v1) readable.
        if let Some(first) = d.displays.first() {
            rec(d.t, 8, &first.brightness);
        }
        for e in &d.displays {
            if let Some(id) = id_of("display", "brightness_pct", &e.name) {
                rec(d.t, id, &e.brightness);
            }
            // Mode dimensions are recorded as explicit Unavailable when the
            // collector did not report them; absence is never promoted to a
            // measured zero, and the display identity is preserved.
            for (metric, val) in [
                ("width", e.width),
                ("height", e.height),
                ("freq_hz", e.freq_hz),
                ("bpp", e.bpp),
            ] {
                if let Some(id) = id_of("display", metric, &e.name) {
                    let ev = match val {
                        Some(v) => measured(v as f64),
                        None => Telemetry::unavailable(
                            UnavailKind::NotSampled,
                            "display mode not reported",
                            "display",
                            ClockStamp::default(),
                        ),
                    };
                    rec(d.t, id, &ev);
                }
            }
            if let Some(id) = id_of("display", "primary", &e.name) {
                rec(d.t, id, &measured(if e.primary { 1.0 } else { 0.0 }));
            }
        }
        for (inst, b) in &d.unmatched_sensors {
            if let Some(id) = id_of("display", "unmatched_brightness", inst) {
                let ev = match b {
                    Some(v) => measured(*v),
                    None => Telemetry::unavailable(
                        UnavailKind::NotSampled,
                        "unmatched sensor brightness not reported",
                        "display",
                        ClockStamp::default(),
                    ),
                };
                rec(d.t, id, &ev);
            }
        }
        for h in &d.hdr_targets {
            let extra = format!("{}|{}", h.adapter, h.target);
            if let Some(id) = id_of("display", "hdr_supported", &extra) {
                rec(d.t, id, &measured(if h.supported { 1.0 } else { 0.0 }));
            }
            if let Some(id) = id_of("display", "hdr_enabled", &extra) {
                rec(d.t, id, &measured(if h.enabled { 1.0 } else { 0.0 }));
            }
        }
    }
    for n in &s.net {
        rec(n.t, 9, &n.total_rx);
        rec(n.t, 10, &n.total_tx);
        if let Some(id) = id_of("net", "topology_stale", "") {
            rec(n.t, id, &measured(if n.topology_stale { 1.0 } else { 0.0 }));
        }
        t_rec(n.t, ID_NET_T_START, n.t_start_ms);
        t_rec(n.t, ID_NET_T_END, n.t_end_ms);
        for (inst, rx, tx) in &n.throughput {
            if let Some(id) = id_of("net", "rx_bps", inst) {
                rec(n.t, id, rx);
            }
            if let Some(id) = id_of("net", "tx_bps", inst) {
                rec(n.t, id, tx);
            }
        }
        for a in &n.adapters {
            for (metric, v) in [
                ("adapter.tx_mbps", &a.tx_mbps),
                ("adapter.rx_mbps", &a.rx_mbps),
                ("adapter.in_octets", &a.in_octets),
                ("adapter.out_octets", &a.out_octets),
            ] {
                if let Some(id) = id_of("net", metric, &a.alias) {
                    rec(n.t, id, v);
                }
            }
            // Adapter state is recorded as explicit Unavailable when not
            // reported, so the adapter identity is never dropped and the
            // loader never sees a fabricated zero.
            for (metric, val) in [("adapter.oper", a.oper), ("adapter.media", a.media)] {
                if let Some(id) = id_of("net", metric, &a.alias) {
                    let ev = match val {
                        Some(v) => measured(v),
                        None => Telemetry::unavailable(
                            UnavailKind::NotSampled,
                            "adapter state not reported",
                            "net",
                            ClockStamp::default(),
                        ),
                    };
                    rec(n.t, id, &ev);
                }
            }
            if let Some(id) = id_of("net", "adapter.admin_up", &a.alias) {
                rec(n.t, id, &measured(if a.admin_up { 1.0 } else { 0.0 }));
            }
        }
        for w in &n.wifi {
            if let Some(id) = id_of("net", "wifi.signal_pct", &w.descr) {
                rec(n.t, id, &w.signal_pct);
            }
            if let Some(id) = id_of("net", "wifi.state", &w.descr) {
                let ev = match w.state {
                    Some(v) => measured(v),
                    None => Telemetry::unavailable(
                        UnavailKind::NotSampled,
                        "wifi state not reported",
                        "net",
                        ClockStamp::default(),
                    ),
                };
                rec(n.t, id, &ev);
            }
        }
    }
    for d in &s.storage {
        rec(d.t, 11, &d.disk_time);
        rec(d.t, 12, &d.read_bps);
        rec(d.t, 13, &d.write_bps);
        t_rec(d.t, ID_STORAGE_T_START, d.t_start_ms);
        t_rec(d.t, ID_STORAGE_T_END, d.t_end_ms);
    }
    for u in &s.usb {
        t_rec(u.t, ID_USB_T_START, u.t_start_ms);
        t_rec(u.t, ID_USB_T_END, u.t_end_ms);
        for c in &u.classes {
            let id = match c.class.as_str() {
                "usb" => Some(14),
                "bluetooth" => Some(15),
                "camera" => Some(16),
                "audio" => Some(17),
                "hid" => Some(18),
                _ => id_of("usb", "count", &c.class),
            };
            // Stored evidence travels intact (prov/kind/quality/reason);
            // never re-synthesized as measured.
            if let Some(id) = id {
                rec(u.t, id, &c.evidence);
            }
        }
        for p in &u.power_devices {
            if let Some(id) = id_of("usb", "power_device", p) {
                rec(u.t, id, &measured(1.0));
            }
        }
    }
    for m in &s.selfmon {
        rec(m.t, 19, &m.cpu_pct);
        rec(m.t, 20, &m.ws_mb);
        rec(m.t, 21, &m.io_write_b);
        rec(m.t, 22, &m.threads);
        rec(m.t, ID_SELF_PRIV_MB, &m.priv_mb);
        rec(m.t, ID_SELF_IO_READ, &m.io_read_b);
        rec(m.t, ID_SELF_CTX_SWITCHES, &m.ctx_switches);
        t_rec(m.t, ID_SELF_T_START, m.t_start_ms);
        t_rec(m.t, ID_SELF_T_END, m.t_end_ms);
    }
    // Timing records were written to the side buffer; append them now.
    samples.extend_from_slice(&samples_t);
    records += t_records;

    /// Telemetry JSON for manifest-embedded values (same envelope shape as
    /// JSONL: no "k" on present values).
    fn telemetry_to_json(t: &Telemetry<f64>) -> String {
        match &t.value {
            Reading::Measured(x) | Reading::Derived(x) | Reading::Estimated(x) => {
                format!(
                    "{{\"v\":{x},\"p\":\"{}\",\"q\":{}}}",
                    t.provenance().as_str(),
                    t.quality as u8
                )
            }
            Reading::Unavailable(r) => {
                format!(
                    "{{\"v\":null,\"p\":\"unavailable\",\"k\":{},\"q\":{},\"reason\":\"{}\"}}",
                    t.unavail_kind.unwrap_or(UnavailKind::NotSampled) as u8,
                    t.quality as u8,
                    esc(r)
                )
            }
        }
    }

    let mut metrics = String::new();
    for (i, (c, m, u)) in STATIC_METRICS.iter().enumerate() {
        metrics.push_str(&format!(
            "{{\"id\":{i},\"collector\":\"{c}\",\"metric\":\"{m}\",\"unit\":\"{u}\",\"extra\":\"\"}},"
        ));
    }
    for d in &dyn_table {
        metrics.push_str(&format!(
            "{{\"id\":{},\"collector\":\"{}\",\"metric\":\"{}\",\"unit\":\"{}\",\"extra\":\"{}\"}},",
            d.id,
            esc(&d.collector),
            esc(&d.metric),
            esc(&d.unit),
            esc(&d.extra)
        ));
    }
    let metrics = metrics.trim_end_matches(',').to_string();
    let footer = s.footer.as_ref().map(to_json).unwrap_or("null".to_string());
    // v2 manifest: identity tables + full policy history + battery meta.
    let mut gpu_table = String::new();
    for (i, (name, discrete)) in gpu_names.iter().enumerate() {
        if i > 0 {
            gpu_table.push(',');
        }
        gpu_table.push_str(&format!(
            "{{\"name\":\"{}\",\"discrete\":{}}}",
            esc(name),
            discrete
        ));
    }
    let mut proc_table = String::new();
    let mut first_proc = true;
    for d in dyn_table.iter().filter(|d| d.collector == "proc") {
        // extra is "pid:name"; identity joins from the first-seen table.
        let (pid_s, _) = d.extra.split_once(':').unwrap_or(("0", &d.extra));
        let pid: u32 = pid_s.parse().unwrap_or(0);
        let (ppid, start, exit) = proc_pids
            .iter()
            .find(|(p, _, _, _, _)| *p == pid)
            .map(|(_, _, pp, s, x)| (*pp, *s, *x))
            .unwrap_or((0, None, None));
        if !first_proc {
            proc_table.push(',');
        }
        first_proc = false;
        let start_json = start.map(|v| v.to_string()).unwrap_or("null".to_string());
        let exit_json = exit.map(|v| v.to_string()).unwrap_or("null".to_string());
        proc_table.push_str(&format!(
            "{{\"metric_id\":{},\"pid\":{pid},\"name\":\"{}\",\"ppid\":{ppid},\"start_unix_ms\":{start_json},\"exit_unix_ms\":{exit_json}}}",
            d.id,
            esc(d.extra.split_once(':').map(|(_, n)| n).unwrap_or(&d.extra))
        ));
    }
    let mut pol_hist = String::new();
    for (i, (t, p)) in s.policy_history.iter().enumerate() {
        if i > 0 {
            pol_hist.push(',');
        }
        let scheme = match &p.scheme_name {
            Some(n) => format!("\"{}\"", esc(n)),
            None => "null".to_string(),
        };
        let guid = match &p.scheme_guid {
            Some(n) => format!("\"{}\"", esc(n)),
            None => "null".to_string(),
        };
        let snap = p
            .policy_snapshot_wall_ms
            .map(|v| v.to_string())
            .unwrap_or("null".to_string());
        pol_hist.push_str(&format!(
            "{{\"t\":{t:.3},\"scheme\":{scheme},\"scheme_guid\":{guid},\"snapshot_wall_ms\":{snap},\
             \"timer_resolution\":{},\"cpu_min_ac\":{},\"cpu_min_dc\":{},\"cpu_max_ac\":{},\"cpu_max_dc\":{},\
             \"epp_ac\":{},\"epp_dc\":{},\"brightness_ac\":{},\"brightness_dc\":{},\
             \"display_timeout_ac\":{},\"display_timeout_dc\":{},\"sleep_timeout_ac\":{},\"sleep_timeout_dc\":{},\
             \"hibernate_timeout_ac\":{},\"hibernate_timeout_dc\":{},\"low_batt_ac\":{},\"low_batt_dc\":{},\
             \"crit_batt_ac\":{},\"crit_batt_dc\":{}}}",
            telemetry_to_json(&p.timer_resolution),
            telemetry_to_json(&p.cpu_min_ac),
            telemetry_to_json(&p.cpu_min_dc),
            telemetry_to_json(&p.cpu_max_ac),
            telemetry_to_json(&p.cpu_max_dc),
            telemetry_to_json(&p.epp_ac),
            telemetry_to_json(&p.epp_dc),
            telemetry_to_json(&p.brightness_ac),
            telemetry_to_json(&p.brightness_dc),
            telemetry_to_json(&p.display_timeout_ac),
            telemetry_to_json(&p.display_timeout_dc),
            telemetry_to_json(&p.sleep_timeout_ac),
            telemetry_to_json(&p.sleep_timeout_dc),
            telemetry_to_json(&p.hibernate_timeout_ac),
            telemetry_to_json(&p.hibernate_timeout_dc),
            telemetry_to_json(&p.low_batt_ac),
            telemetry_to_json(&p.low_batt_dc),
            telemetry_to_json(&p.crit_batt_ac),
            telemetry_to_json(&p.crit_batt_dc),
        ));
    }
    // Network interface identity (first-seen) and Wi-Fi SSID identity, so
    // per-interface throughput remains interpretable after reload.
    let mut net_ident: Vec<(String, String, String, String)> = Vec::new();
    for n in &s.net {
        for a in &n.adapters {
            if !net_ident.iter().any(|(al, _, _, _)| al == &a.alias) {
                net_ident.push((
                    a.alias.clone(),
                    a.descr.clone(),
                    a.class.clone(),
                    a.iftype.clone(),
                ));
            }
        }
    }
    let net_table = net_ident
        .iter()
        .map(|(al, d, c, i)| {
            format!(
                "{{\"alias\":\"{}\",\"descr\":\"{}\",\"class\":\"{}\",\"iftype\":\"{}\"}}",
                esc(al),
                esc(d),
                esc(c),
                esc(i)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let mut wifi_ident: Vec<(String, String)> = Vec::new();
    for n in &s.net {
        for w in &n.wifi {
            if !wifi_ident.iter().any(|(d, _)| d == &w.descr) {
                wifi_ident.push((w.descr.clone(), w.ssid.clone()));
            }
        }
    }
    let wifi_table = wifi_ident
        .iter()
        .map(|(d, ss)| format!("{{\"descr\":\"{}\",\"ssid\":\"{}\"}}", esc(d), esc(ss)))
        .collect::<Vec<_>>()
        .join(",");
    let bat_json = |v: Option<f64>| v.map(|x| format!("{x}")).unwrap_or("null".to_string());
    let batu_json = |v: Option<u32>| v.map(|x| format!("{x}")).unwrap_or("null".to_string());
    let bat_list = format!(
        "[{}]",
        s.battery_meta
            .batteries
            .iter()
            .map(|e| format!(
                "{{\"index\":{},\"designed_mwh\":{},\"full_mwh\":{},\"cycle_count\":{},\
                 \"chemistry\":\"{}\",\"device_name\":\"{}\",\"unique_id\":\"{}\",\
                 \"technology\":{},\"capabilities\":{},\"temperature_raw\":{},\
                 \"mfg_name\":\"{}\",\"mfg_date\":\"{}\",\
                 \"designed_prov\":\"{}\",\"full_prov\":\"{}\"}}",
                e.index,
                bat_json(e.designed_mwh),
                bat_json(e.full_mwh),
                batu_json(e.cycle_count),
                esc(e.chemistry.as_deref().unwrap_or("")),
                esc(e.device_name.as_deref().unwrap_or("")),
                esc(e.unique_id.as_deref().unwrap_or("")),
                batu_json(e.technology),
                batu_json(e.capabilities),
                batu_json(e.temperature_raw),
                esc(e.mfg_name.as_deref().unwrap_or("")),
                esc(e.mfg_date.as_deref().unwrap_or("")),
                esc(&e.design_prov),
                esc(&e.full_prov),
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let source_hash = source_hash_hex(label, s.wall_base_ms, records);
    let manifest = format!(
        "{{\"tool\":\"power-forensics\",\"tool_version\":\"{}\",\"machine_id\":\"{}\",\"source_hash\":\"{source_hash}\",\
         \"format\":\"{V2_FORMAT}\",\"schema\":{V2_SCHEMA},\"label\":\"{}\",\
         \"source\":\"{}\",\"wall_base_ms\":{},\"metrics\":[{metrics}],\
         \"gpu_adapters\":[{gpu_table}],\"proc_identity\":[{proc_table}],\
         \"net_adapters\":[{net_table}],\"wifi_identity\":[{wifi_table}],\
         \"policy_history\":[{pol_hist}],\
         \"battery\":{{\"full_mwh\":{},\"design_mwh\":{},\"index\":{},\"count\":{},\
         \"full_prov\":\"{}\",\"design_prov\":\"{}\",\"batteries\":{bat_list}}},\
         \"footer\":{footer},\
         \"limitations\":[\"gpu awake confidence/evidence and display/usb change text are presentation-only\",\"wifi SSID and adapter descriptions are archived as identity tables, not per-tick\"]}}",
        env!("CARGO_PKG_VERSION"),
        esc(&machine_id()),
        esc(label),
        esc(source),
        s.wall_base_ms,
        bat_json(s.battery_meta.full_mwh),
        bat_json(s.battery_meta.design_mwh),
        batu_json(s.battery_meta.battery_index),
        batu_json(s.battery_meta.battery_count),
        esc(&s.battery_meta.full_prov),
        esc(&s.battery_meta.design_prov),
    );
    let events: Vec<String> = s
        .events
        .iter()
        .map(|e| {
            format!(
                "{{\"type\":\"event\",\"wall_ms\":{},\"mono_ms\":{},\"kind\":\"{}\",\"detail\":\"{}\"}}",
                e.wall_ms,
                e.mono_ms,
                esc(&e.kind),
                esc(&e.detail)
            )
        })
        .collect();
    Archive {
        manifest,
        samples,
        events,
        records,
    }
}

/// Load an archive back into SessionData (policy stays empty by design).
pub fn load_archive(
    label: &str,
    manifest_text: &str,
    samples: &[u8],
    events: &[String],
) -> Result<SessionData, String> {
    let manifest = parse(manifest_text).map_err(|e| format!("bad manifest: {e}"))?;
    if manifest.get("format").and_then(|f| f.as_str()) != Some(V1_FORMAT) {
        return Err("unsupported archive format".to_string());
    }
    // id -> (collector, metric, extra)
    let mut table: HashMap<u16, (String, String, String)> = HashMap::new();
    if let Some(list) = manifest.get("metrics").and_then(|m| m.arr()) {
        for m in list {
            let (Some(id), Some(c), Some(mt), Some(e)) = (
                m.get("id").and_then(|v| v.num()),
                m.get("collector").and_then(|v| v.as_str()),
                m.get("metric").and_then(|v| v.as_str()),
                m.get("extra").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if table
                .insert(id as u16, (c.to_string(), mt.to_string(), e.to_string()))
                .is_some()
            {
                return Err(format!("duplicate metric id {id} in manifest"));
            }
        }
    }
    let mut s = SessionData {
        label: label.to_string(),
        ..Default::default()
    };
    // Aggregation maps for grouped collectors (value + stored prov byte).
    let mut gpu_map: HashMap<(u64, String), (f64, u8)> = HashMap::new();
    let mut proc_map: HashMap<(u64, u32, String), (f64, u8)> = HashMap::new();
    let mut scalar: HashMap<(u64, u16), (f64, u8)> = HashMap::new();
    let mut times: Vec<u64> = Vec::new();
    if !samples.len().is_multiple_of(RECORD_LEN) {
        return Err("samples.bin length is not a multiple of 19".to_string());
    }
    for chunk in samples.as_chunks::<RECORD_LEN>().0 {
        let (mono, id, prov, val) = (
            get_u64(chunk, 0).ok_or("bad record")?,
            get_u16(chunk, 8).ok_or("bad record")?,
            chunk[10],
            get_f64(chunk, 11).ok_or("bad record")?,
        );
        let Some((collector, metric, extra)) = table.get(&id) else {
            continue; // unknown metric: skip, never fabricate
        };
        if !times.contains(&mono) {
            times.push(mono);
        }
        match (collector.as_str(), metric.as_str()) {
            ("gpu", _) => {
                gpu_map.insert((mono, extra.clone()), (val, prov));
            }
            ("proc", _) => {
                let (pid, name) = extra.split_once(':').unwrap_or(("0", extra));
                proc_map.insert(
                    (mono, pid.parse().unwrap_or(0), name.to_string()),
                    (val, prov),
                );
            }
            _ => {
                scalar.insert((mono, id), (val, prov));
            }
        }
    }
    times.sort_unstable();
    fn collector_source(name: &str) -> &'static str {
        match name {
            "battery" => "battery",
            "cpu" => "cpu",
            "gpu" => "gpu",
            "proc" => "proc",
            "display" => "display",
            "net" => "net",
            "storage" => "storage",
            "usb" => "usb",
            "self" => "self",
            "os" => "os",
            _ => "",
        }
    }
    // Rebuild evidence from v1 records. The stored prov byte is trusted
    // for 0/1/2 (migrate wrote it from live provenance); unknown codes
    // demote to Unavailable (mirroring v2), never to Measured. Quality is
    // Unknown and wall time is 0 (v1 never stored either).
    // v1 helper: present value + prov code -> evidence (demotes unknowns).
    let tele_val = |v: f64, prov: u8, source: &'static str, stamp: ClockStamp| -> Telemetry<f64> {
        match prov {
            0 => Telemetry {
                value: Reading::Measured(v),
                source,
                stamp,
                quality: SampleQuality::Unknown,
                unavail_kind: None,
            },
            1 => Telemetry {
                value: Reading::Derived(v),
                source,
                stamp,
                quality: SampleQuality::Unknown,
                unavail_kind: None,
            },
            2 => Telemetry {
                value: Reading::Estimated(v),
                source,
                stamp,
                quality: SampleQuality::Unknown,
                unavail_kind: None,
            },
            _ => Telemetry {
                value: Reading::Unavailable(crate::session::intern_reason(
                    "unknown provenance code",
                )),
                source,
                stamp,
                quality: SampleQuality::Unknown,
                unavail_kind: Some(UnavailKind::NotSampled),
            },
        }
    };
    let tele = |mono: u64, id: u16, source: &'static str| -> Telemetry<f64> {
        let stamp = ClockStamp {
            wall_millis: 0,
            mono_millis: mono,
        };
        match scalar.get(&(mono, id)).copied() {
            Some((x, prov)) => tele_val(x, prov, source, stamp),
            None => Telemetry::unavailable(UnavailKind::NotSampled, "not archived", source, stamp),
        }
    };
    let g = |mono: u64, id: u16| {
        tele(
            mono,
            id,
            collector_source(table.get(&id).map(|(c, _, _)| c.as_str()).unwrap_or("")),
        )
    };
    for &mono in &times {
        let t = mono as f64 / 1000.0;
        s.battery.push(crate::session::BatteryPoint {
            t,
            discharge: g(mono, 0),
            charge: g(mono, 1),
            remaining_wh: g(mono, 2),
            pct: g(mono, 3),
        });
        s.cpu.push(crate::session::CpuPoint {
            t,
            utility: g(mono, 4),
            c3_pct: g(mono, 5),
            idle_breaks: g(mono, 6),
            pkg_derived_w: g(mono, 7),
            ..Default::default()
        });
        s.display.push(crate::session::DisplayPoint {
            t,
            brightness: g(mono, 8),
            ..Default::default()
        });
        s.net.push(crate::session::NetPoint {
            t,
            total_rx: g(mono, 9),
            total_tx: g(mono, 10),
            ..Default::default()
        });
        s.storage.push(crate::session::StoragePoint {
            t,
            disk_time: g(mono, 11),
            read_bps: g(mono, 12),
            write_bps: g(mono, 13),
            ..Default::default()
        });
        let mut classes = Vec::new();
        for (class, id) in [
            ("usb", 14),
            ("bluetooth", 15),
            ("camera", 16),
            ("audio", 17),
            ("hid", 18),
        ] {
            if let Some(&(v, prov)) = scalar.get(&(mono, id)) {
                let stamp = ClockStamp {
                    wall_millis: 0,
                    mono_millis: mono,
                };
                let evidence = tele_val(v, prov, "usb", stamp);
                let count = evidence.val().map(|x| x as usize).unwrap_or(0);
                classes.push(crate::session::UsbClassCount {
                    class: class.to_string(),
                    count,
                    evidence,
                });
            }
        }
        if !classes.is_empty() {
            s.usb.push(crate::session::UsbPoint {
                t,
                classes,
                ..Default::default()
            });
        }
        let adapters: Vec<crate::session::GpuAdapterPoint> = gpu_map
            .iter()
            .filter(|((m, _), _)| *m == mono)
            .map(|((_, name), &(v, prov))| {
                let stamp = ClockStamp {
                    wall_millis: 0,
                    mono_millis: mono,
                };
                let total_util = tele_val(v, prov, "gpu", stamp);
                crate::session::GpuAdapterPoint {
                    name: name.clone(),
                    // v1 manifest carries no adapter metadata (fixed in v2).
                    discrete: false,
                    total_util,
                    mem_dedicated_mb: Telemetry::unavailable(
                        UnavailKind::NotSampled,
                        "not archived",
                        "gpu",
                        stamp,
                    ),
                    ..Default::default()
                }
            })
            .collect();
        if !adapters.is_empty() {
            s.gpu.push(crate::session::GpuPoint {
                t,
                adapters,
                ..Default::default()
            });
        }
        let mut top: Vec<crate::session::ProcEntry> = proc_map
            .iter()
            .filter(|((m, _, _), _)| *m == mono)
            .map(|((_, pid, name), &(v, prov))| {
                let stamp = ClockStamp {
                    wall_millis: 0,
                    mono_millis: mono,
                };
                let cpu = tele_val(v, prov, "proc", stamp);
                crate::session::ProcEntry {
                    pid: *pid,
                    ppid: 0,
                    name: name.clone(),
                    start_unix_ms: None,
                    // v1 identity table carries no exit timestamp (added in
                    // the current manifest): None is unknown, not exited.
                    exit_unix_ms: None,
                    cpu,
                }
            })
            .collect();
        top.sort_by_key(|e| e.pid);
        if !top.is_empty() {
            s.procs.push(crate::session::ProcPoint {
                t,
                top,
                ..Default::default()
            });
        }
    }
    for line in events {
        if let Ok(v) = parse(line) {
            s.events.push(crate::session::EventRec {
                wall_ms: v.get("wall_ms").and_then(|m| m.num()).unwrap_or(0.0) as u64,
                mono_ms: v.get("mono_ms").and_then(|m| m.num()).unwrap_or(0.0) as u64,
                kind: v
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("?")
                    .to_string(),
                detail: v
                    .get("detail")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    if let Some(footer) = manifest.get("footer")
        && !matches!(footer, JVal::Null)
    {
        s.footer = Some(footer.clone());
    }
    Ok(s)
}

/// Load a v2 archive with full fidelity: value, provenance, kind, reason,
/// quality, both timestamps, policy history, identity tables, battery
/// metadata, and events. Unknown prov codes demote to Unavailable (never
/// to Measured).
pub fn load_archive_v2(
    label: &str,
    manifest_text: &str,
    samples: &[u8],
    events: &[String],
) -> Result<SessionData, String> {
    let manifest = parse(manifest_text).map_err(|e| format!("bad manifest: {e}"))?;
    if manifest.get("format").and_then(|f| f.as_str()) != Some(V2_FORMAT) {
        return Err("not a pf-archive-2 manifest".to_string());
    }
    let mut table: HashMap<u16, (String, String, String)> = HashMap::new();
    // Manifest metric order is deterministic; keep it so reconstructed
    // per-entity lists (adapters, displays, cores) preserve interface order.
    let mut metric_order: Vec<u16> = Vec::new();
    if let Some(list) = manifest.get("metrics").and_then(|m| m.arr()) {
        for m in list {
            let (Some(id), Some(c), Some(mt), Some(e)) = (
                m.get("id").and_then(|v| v.num()),
                m.get("collector").and_then(|v| v.as_str()),
                m.get("metric").and_then(|v| v.as_str()),
                m.get("extra").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if table
                .insert(id as u16, (c.to_string(), mt.to_string(), e.to_string()))
                .is_some()
            {
                return Err(format!("duplicate metric id {id} in manifest"));
            }
            metric_order.push(id as u16);
        }
    }
    fn collector_source(name: &str) -> &'static str {
        match name {
            "battery" => "battery",
            "cpu" => "cpu",
            "gpu" => "gpu",
            "proc" => "proc",
            "display" => "display",
            "net" => "net",
            "storage" => "storage",
            "usb" => "usb",
            "self" => "self",
            "os" => "os",
            _ => "",
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn tele(
        mono: u64,
        wall: u64,
        prov: u8,
        kind: u8,
        quality: u8,
        val: Option<f64>,
        reason: &str,
        source: &'static str,
    ) -> Telemetry<f64> {
        let stamp = ClockStamp {
            wall_millis: wall,
            mono_millis: mono,
        };
        let q = SampleQuality::from_u8(quality);
        let k = UnavailKind::from_u8(kind);
        match (val, prov) {
            (Some(x), 0) => Telemetry {
                value: Reading::Measured(x),
                source,
                stamp,
                quality: if q == SampleQuality::Unknown {
                    SampleQuality::Fresh
                } else {
                    q
                },
                unavail_kind: None,
            },
            (Some(x), 1) => Telemetry {
                value: Reading::Derived(x),
                source,
                stamp,
                quality: if q == SampleQuality::Unknown {
                    SampleQuality::Fresh
                } else {
                    q
                },
                unavail_kind: None,
            },
            (Some(x), 2) => Telemetry {
                value: Reading::Estimated(x),
                source,
                stamp,
                quality: if q == SampleQuality::Unknown {
                    SampleQuality::Fresh
                } else {
                    q
                },
                unavail_kind: None,
            },
            // Present value with an unknown prov code, or absent value:
            // Unavailable with the stored reason. Never Measured.
            _ => Telemetry {
                value: Reading::Unavailable(crate::session::intern_reason(reason)),
                source,
                stamp,
                quality: q,
                unavail_kind: Some(k),
            },
        }
    }
    let mut s = SessionData {
        label: label.to_string(),
        ..Default::default()
    };
    s.wall_base_ms = manifest
        .get("wall_base_ms")
        .and_then(|v| v.num())
        .unwrap_or(0.0) as u64;
    // Identity tables.
    let mut gpu_disc: HashMap<String, bool> = HashMap::new();
    if let Some(list) = manifest.get("gpu_adapters").and_then(|m| m.arr()) {
        for g in list {
            if let (Some(n), Some(d)) = (
                g.get("name").and_then(|v| v.as_str()),
                g.get("discrete").and_then(|v| v.as_bool()),
            ) {
                gpu_disc.insert(n.to_string(), d);
            }
        }
    }
    #[allow(clippy::type_complexity)]
    let mut proc_ident: HashMap<u16, (u32, String, u32, Option<u64>, Option<u64>)> = HashMap::new();
    if let Some(list) = manifest.get("proc_identity").and_then(|m| m.arr()) {
        for p in list {
            if let (Some(id), Some(pid)) = (
                p.get("metric_id").and_then(|v| v.num()),
                p.get("pid").and_then(|v| v.num()),
            ) {
                proc_ident.insert(
                    id as u16,
                    (
                        pid as u32,
                        p.get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string(),
                        p.get("ppid").and_then(|v| v.num()).unwrap_or(0.0) as u32,
                        p.get("start_unix_ms")
                            .and_then(|v| v.num())
                            .map(|v| v as u64),
                        // Absent in older manifests: unknown, not exited.
                        p.get("exit_unix_ms")
                            .and_then(|v| v.num())
                            .map(|v| v as u64),
                    ),
                );
            }
        }
    }
    // NOTE: proc_identity is written by migrate below; v2 readers tolerate
    // its absence (identity falls back to the metric extra field).
    // Interface identity table (alias -> descr/class/iftype).
    let mut net_ident: HashMap<String, (String, String, String)> = HashMap::new();
    if let Some(list) = manifest.get("net_adapters").and_then(|m| m.arr()) {
        for a in list {
            if let Some(alias) = a.get("alias").and_then(|v| v.as_str()) {
                net_ident.insert(
                    alias.to_string(),
                    (
                        a.get("descr")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        a.get("class")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        a.get("iftype")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                );
            }
        }
    }
    let mut wifi_ssid: HashMap<String, String> = HashMap::new();
    if let Some(list) = manifest.get("wifi_identity").and_then(|m| m.arr()) {
        for w in list {
            if let Some(descr) = w.get("descr").and_then(|v| v.as_str()) {
                wifi_ssid.insert(
                    descr.to_string(),
                    w.get("ssid")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                );
            }
        }
    }
    // Single record store: grouping is by manifest (collector, metric,
    // extra) metadata, so no collector can silently drop a series.
    let mut scalar: HashMap<(u64, u16), Telemetry<f64>> = HashMap::new();
    let mut times: Vec<(u64, u64)> = Vec::new(); // (mono, wall)
    let mut id_of: HashMap<(String, String, String), u16> = HashMap::new();
    for (id, (c, m, e)) in &table {
        id_of.insert((c.clone(), m.clone(), e.clone()), *id);
    }
    let mut off = 0usize;
    let mut record_count = 0usize;
    while off < samples.len() {
        let (mono, wall, id, prov, kind, quality, val, reason, next) =
            get_rec(samples, off).ok_or("truncated v2 record".to_string())?;
        off = next;
        record_count += 1;
        let Some((collector, _metric, _extra)) = table.get(&id) else {
            continue; // unknown metric: skip, never fabricate
        };
        let source = collector_source(collector);
        if !times.iter().any(|(m, _)| *m == mono) {
            times.push((mono, wall));
        }
        // Reason strings in v2 records are raw; interning happens in tele().
        let t = tele(mono, wall, prov, kind, quality, val, &reason, source);
        scalar.insert((mono, id), t);
    }
    times.sort_unstable();
    let g = |mono: u64, id: u16| -> Telemetry<f64> {
        scalar.get(&(mono, id)).cloned().unwrap_or_else(|| {
            Telemetry::unavailable(
                UnavailKind::NotSampled,
                "not archived",
                collector_source(table.get(&id).map(|(c, _, _)| c.as_str()).unwrap_or("")),
                ClockStamp {
                    wall_millis: times
                        .iter()
                        .find(|(m, _)| *m == mono)
                        .map(|(_, w)| *w)
                        .unwrap_or(0),
                    mono_millis: mono,
                },
            )
        })
    };
    // Optional acquisition window: present only when a record was written.
    // Missing stays None (never a fabricated 0).
    let tms = |mono: u64, id: u16| -> Option<u64> { g(mono, id).val().map(|v| v as u64) };
    // Dynamic lookups: (collector, metric, extra) -> id, and ordered extras.
    let dynamic = |mono: u64, collector: &str, metric: &str, extra: &str| -> Telemetry<f64> {
        id_of
            .get(&(collector.to_string(), metric.to_string(), extra.to_string()))
            .and_then(|id| scalar.get(&(mono, *id)).cloned())
            .unwrap_or_else(|| {
                Telemetry::unavailable(
                    UnavailKind::NotSampled,
                    "not archived",
                    collector_source(collector),
                    ClockStamp {
                        wall_millis: times
                            .iter()
                            .find(|(m, _)| *m == mono)
                            .map(|(_, w)| *w)
                            .unwrap_or(0),
                        mono_millis: mono,
                    },
                )
            })
    };
    let present = |mono: u64, collector: &str, metric: &str, extra: &str| -> bool {
        id_of
            .get(&(collector.to_string(), metric.to_string(), extra.to_string()))
            .map(|id| scalar.contains_key(&(mono, *id)))
            .unwrap_or(false)
    };
    let extra_list = |collector: &str, metric: &str| -> Vec<String> {
        metric_order
            .iter()
            .filter(|id| **id >= DYNAMIC_BASE)
            .filter_map(|id| table.get(id))
            .filter(|(c, m, _)| c == collector && m == metric)
            .map(|(_, _, e)| e.clone())
            .collect()
    };
    for &(mono, _) in &times {
        let t = mono as f64 / 1000.0;
        // Presence-gated: a collector point exists iff at least one of its
        // records exists at this tick. This mirrors JSONL semantics (one
        // point per line) instead of fabricating placeholder points.
        let has = |ids: &[u16]| ids.iter().any(|id| scalar.contains_key(&(mono, *id)));
        if has(&[0, 1, 2, 3]) {
            s.battery.push(crate::session::BatteryPoint {
                t,
                discharge: g(mono, 0),
                charge: g(mono, 1),
                remaining_wh: g(mono, 2),
                pct: g(mono, 3),
            });
        }
        if has(&[
            ID_BATTERY_AC,
            ID_BATTERY_RATE_MW,
            ID_BATTERY_HEALTH,
            ID_BATTERY_TEMP_RAW,
            ID_BATTERY_T_START,
            ID_BATTERY_T_END,
        ]) {
            s.battery_extra.push(crate::session::BatteryExtraPoint {
                t,
                ac: g(mono, ID_BATTERY_AC),
                rate_mw: g(mono, ID_BATTERY_RATE_MW),
                health: g(mono, ID_BATTERY_HEALTH),
                temperature_raw: g(mono, ID_BATTERY_TEMP_RAW),
                t_start_ms: tms(mono, ID_BATTERY_T_START),
                t_end_ms: tms(mono, ID_BATTERY_T_END),
            });
        }
        if has(&[
            4,
            5,
            6,
            7,
            ID_CPU_PKG_POWER,
            ID_CPU_CTX_SWITCHES,
            ID_CPU_T_START,
            ID_CPU_T_END,
        ]) {
            let cores: Vec<crate::session::CpuCorePoint> = extra_list("cpu", "core.utility")
                .into_iter()
                .filter(|inst| present(mono, "cpu", "core.utility", inst))
                .map(|inst| crate::session::CpuCorePoint {
                    utility: dynamic(mono, "cpu", "core.utility", &inst),
                    performance: dynamic(mono, "cpu", "core.performance", &inst),
                    freq_mhz: dynamic(mono, "cpu", "core.freq_mhz", &inst),
                    parking: dynamic(mono, "cpu", "core.parking", &inst),
                    instance: inst,
                })
                .collect();
            let totals: Vec<(String, Telemetry<f64>)> = metric_order
                .iter()
                .filter_map(|id| table.get(id).map(|(c, m, _)| (c.clone(), m.clone())))
                .filter(|(c, m)| c == "cpu" && m.starts_with("total."))
                .map(|(_, m)| {
                    let key = m.strip_prefix("total.").unwrap_or(&m).to_string();
                    let v = dynamic(mono, "cpu", &m, "");
                    (key, v)
                })
                .collect();
            s.cpu.push(crate::session::CpuPoint {
                t,
                t_start_ms: tms(mono, ID_CPU_T_START),
                t_end_ms: tms(mono, ID_CPU_T_END),
                utility: g(mono, 4),
                c3_pct: g(mono, 5),
                idle_breaks: g(mono, 6),
                pkg_derived_w: g(mono, 7),
                pkg_power_w: g(mono, ID_CPU_PKG_POWER),
                ctx_switches: g(mono, ID_CPU_CTX_SWITCHES),
                cores,
                totals,
            });
        }
        let mut adapters: Vec<crate::session::GpuAdapterPoint> = Vec::new();
        for name in extra_list("gpu", "total_util_pct") {
            if !present(mono, "gpu", "total_util_pct", &name) {
                continue;
            }
            let at = |metric: &str| dynamic(mono, "gpu", metric, &name);
            let awake = id_of
                .get(&("gpu".to_string(), "awake".to_string(), name.clone()))
                .and_then(|id| scalar.get(&(mono, *id)))
                .map(|ev| crate::session::AwakeRec {
                    active: ev.val().unwrap_or(0.0) >= 0.5,
                    // Confidence/evidence are presentation-only.
                    confidence: String::new(),
                    evidence: String::new(),
                });
            let top_pids: Vec<(u32, Telemetry<f64>)> = extra_list("gpu", "top_pid_util_pct")
                .into_iter()
                .filter(|k| {
                    k.split_once('|').map(|(n, _)| n == name).unwrap_or(false)
                        && present(mono, "gpu", "top_pid_util_pct", k)
                })
                .filter_map(|k| {
                    let (_, p) = k.split_once('|')?;
                    let v = dynamic(mono, "gpu", "top_pid_util_pct", &k);
                    Some((p.parse().unwrap_or(0), v))
                })
                .collect();
            adapters.push(crate::session::GpuAdapterPoint {
                name: name.clone(),
                discrete: gpu_disc.get(&name).copied().unwrap_or(false),
                total_util: at("total_util_pct"),
                mem_dedicated_mb: at("mem_dedicated_mb"),
                mem_shared_mb: at("mem_shared_mb"),
                util_3d: at("util_3d"),
                util_compute: at("util_compute"),
                util_decode: at("util_decode"),
                util_codec: at("util_codec"),
                util_copy: at("util_copy"),
                util_other: at("util_other"),
                engines_active: at("engines_active"),
                engines_seen: at("engines_seen"),
                awake,
                top_pids,
            });
        }
        if !adapters.is_empty() {
            let stale = id_of
                .get(&("gpu".to_string(), "stale".to_string(), String::new()))
                .and_then(|id| scalar.get(&(mono, *id)))
                .map(|ev| ev.val().unwrap_or(0.0) >= 0.5)
                .unwrap_or(false);
            s.gpu.push(crate::session::GpuPoint {
                t,
                t_start_ms: tms(mono, ID_GPU_T_START),
                t_end_ms: tms(mono, ID_GPU_T_END),
                stale,
                adapters,
            });
        }
        // Process identity: manifest table first, metric extra fallback.
        let mut top: Vec<crate::session::ProcEntry> = metric_order
            .iter()
            .filter_map(|id| table.get(id).map(|(c, m, _)| (*id, c.clone(), m.clone())))
            .filter(|(_, c, m)| c == "proc" && m == "cpu_pct")
            .filter_map(|(id, _, _)| {
                let cpu = scalar.get(&(mono, id))?.clone();
                Some(if let Some((pid, name, _, _, _)) = proc_ident.get(&id) {
                    let (ppid, start, ident_exit) = proc_ident
                        .get(&id)
                        .map(|(_, _, p, s, x)| (*p, *s, *x))
                        .unwrap_or((0, None, None));
                    // Per-tick exit wins over the first-seen identity.
                    let tick_exit = id_of
                        .get(&(
                            "proc".to_string(),
                            "exit_unix_ms".to_string(),
                            format!("{pid}:{name}"),
                        ))
                        .and_then(|eid| scalar.get(&(mono, *eid)))
                        .and_then(|t| t.val())
                        .map(|v| v as u64);
                    crate::session::ProcEntry {
                        pid: *pid,
                        ppid,
                        name: name.clone(),
                        start_unix_ms: start,
                        exit_unix_ms: tick_exit.or(ident_exit),
                        cpu,
                    }
                } else {
                    let extra = table
                        .get(&id)
                        .map(|(_, _, e)| e.clone())
                        .unwrap_or_default();
                    let (p, n) = extra.split_once(':').unwrap_or(("0", &extra));
                    crate::session::ProcEntry {
                        pid: p.parse().unwrap_or(0),
                        ppid: 0,
                        name: n.to_string(),
                        start_unix_ms: None,
                        exit_unix_ms: None,
                        cpu,
                    }
                })
            })
            .collect();
        top.sort_by_key(|e| e.pid);
        if !top.is_empty() {
            // Coverage metadata: absent records reload as None (unknown).
            let cov = |metric: &str| dynamic(mono, "proc", metric, "").val();
            s.procs.push(crate::session::ProcPoint {
                t,
                t_start_ms: tms(mono, ID_PROC_T_START),
                t_end_ms: tms(mono, ID_PROC_T_END),
                total_threads: cov("coverage.total_threads").map(|v| v as u64),
                inaccessible: cov("coverage.inaccessible").map(|v| v as usize),
                truncated: cov("coverage.truncated").map(|v| v >= 0.5),
                top,
            });
        }
        // Every display, unmatched sensor, and HDR target.
        let mut displays: Vec<crate::session::DisplayEntry> = Vec::new();
        for name in extra_list("display", "brightness_pct") {
            if !present(mono, "display", "brightness_pct", &name) {
                continue;
            }
            let dnum = |metric: &str| dynamic(mono, "display", metric, &name).val();
            let du32 = |metric: &str| dnum(metric).map(|v| v as u32);
            displays.push(crate::session::DisplayEntry {
                name: name.clone(),
                primary: dnum("primary").map(|v| v >= 0.5).unwrap_or(false),
                width: du32("width"),
                height: du32("height"),
                freq_hz: du32("freq_hz"),
                bpp: du32("bpp"),
                brightness: dynamic(mono, "display", "brightness_pct", &name),
            });
        }
        // Older v2 archives stored only static id 8 (first display).
        if displays.is_empty() && scalar.contains_key(&(mono, 8)) {
            displays.push(crate::session::DisplayEntry {
                name: "0".to_string(),
                brightness: g(mono, 8),
                ..Default::default()
            });
        }
        let unmatched: Vec<(String, Option<f64>)> = extra_list("display", "unmatched_brightness")
            .into_iter()
            .filter(|inst| present(mono, "display", "unmatched_brightness", inst))
            .map(|inst| {
                let v = dynamic(mono, "display", "unmatched_brightness", &inst).val();
                (inst, v)
            })
            .collect();
        let hdr: Vec<crate::session::HdrTargetRec> = extra_list("display", "hdr_supported")
            .into_iter()
            .filter(|extra| present(mono, "display", "hdr_supported", extra))
            .map(|extra| {
                let (adapter, target) = extra.split_once('|').unwrap_or((&extra, "0"));
                crate::session::HdrTargetRec {
                    adapter: adapter.to_string(),
                    target: target.parse().unwrap_or(0),
                    supported: dynamic(mono, "display", "hdr_supported", &extra)
                        .val()
                        .unwrap_or(0.0)
                        >= 0.5,
                    enabled: dynamic(mono, "display", "hdr_enabled", &extra)
                        .val()
                        .unwrap_or(0.0)
                        >= 0.5,
                }
            })
            .collect();
        if !displays.is_empty() || !unmatched.is_empty() || !hdr.is_empty() {
            let brightness = displays
                .first()
                .map(|d| d.brightness.clone())
                .unwrap_or_else(|| g(mono, 8));
            s.display.push(crate::session::DisplayPoint {
                t,
                t_start_ms: tms(mono, ID_DISPLAY_T_START),
                t_end_ms: tms(mono, ID_DISPLAY_T_END),
                brightness,
                displays,
                unmatched_sensors: unmatched,
                hdr_targets: hdr,
                hdr_error: None,
                changes: Vec::new(),
            });
        }
        // Interface throughput + identity + Wi-Fi state.
        let mut net_adapters: Vec<crate::session::NetAdapterRec> = Vec::new();
        for alias in extra_list("net", "adapter.oper") {
            if !present(mono, "net", "adapter.oper", &alias) {
                continue;
            }
            let (descr, class, iftype) = net_ident.get(&alias).cloned().unwrap_or_default();
            let anum = |metric: &str| dynamic(mono, "net", metric, &alias).val();
            net_adapters.push(crate::session::NetAdapterRec {
                alias: alias.clone(),
                descr,
                class,
                iftype,
                oper: anum("adapter.oper"),
                media: anum("adapter.media"),
                admin_up: anum("adapter.admin_up").map(|v| v >= 0.5).unwrap_or(false),
                tx_mbps: dynamic(mono, "net", "adapter.tx_mbps", &alias),
                rx_mbps: dynamic(mono, "net", "adapter.rx_mbps", &alias),
                in_octets: dynamic(mono, "net", "adapter.in_octets", &alias),
                out_octets: dynamic(mono, "net", "adapter.out_octets", &alias),
            });
        }
        let throughput: Vec<(String, Telemetry<f64>, Telemetry<f64>)> = extra_list("net", "rx_bps")
            .into_iter()
            .filter(|inst| present(mono, "net", "rx_bps", inst))
            .map(|inst| {
                let rx = dynamic(mono, "net", "rx_bps", &inst);
                let tx = dynamic(mono, "net", "tx_bps", &inst);
                (inst, rx, tx)
            })
            .collect();
        let wifi: Vec<crate::session::WifiRec> = extra_list("net", "wifi.signal_pct")
            .into_iter()
            .filter(|descr| present(mono, "net", "wifi.signal_pct", descr))
            .map(|descr| crate::session::WifiRec {
                ssid: wifi_ssid.get(&descr).cloned().unwrap_or_default(),
                state: dynamic(mono, "net", "wifi.state", &descr).val(),
                signal_pct: dynamic(mono, "net", "wifi.signal_pct", &descr),
                descr,
            })
            .collect();
        if has(&[9, 10]) || !net_adapters.is_empty() || !throughput.is_empty() || !wifi.is_empty() {
            s.net.push(crate::session::NetPoint {
                t,
                t_start_ms: tms(mono, ID_NET_T_START),
                t_end_ms: tms(mono, ID_NET_T_END),
                total_rx: g(mono, 9),
                total_tx: g(mono, 10),
                adapters: net_adapters,
                throughput,
                wifi,
                changes: Vec::new(),
                topology_stale: dynamic(mono, "net", "topology_stale", "")
                    .val()
                    .map(|v| v > 0.5)
                    .unwrap_or(false),
            });
        }
        if has(&[11, 12, 13]) {
            s.storage.push(crate::session::StoragePoint {
                t,
                t_start_ms: tms(mono, ID_STORAGE_T_START),
                t_end_ms: tms(mono, ID_STORAGE_T_END),
                disk_time: g(mono, 11),
                read_bps: g(mono, 12),
                write_bps: g(mono, 13),
            });
        }
        let mut classes = Vec::new();
        for (class, id) in [
            ("usb", 14),
            ("bluetooth", 15),
            ("camera", 16),
            ("audio", 17),
            ("hid", 18),
        ] {
            // Stored evidence restored intact (prov/kind/quality/reason);
            // count follows the stored value, 0 when absent.
            if let Some(ev) = scalar.get(&(mono, id)) {
                classes.push(crate::session::UsbClassCount {
                    class: class.to_string(),
                    count: ev.val().map(|x| x as usize).unwrap_or(0),
                    evidence: ev.clone(),
                });
            }
        }
        for class in extra_list("usb", "count") {
            if !present(mono, "usb", "count", &class) {
                continue;
            }
            let ev = dynamic(mono, "usb", "count", &class);
            classes.push(crate::session::UsbClassCount {
                count: ev.val().map(|x| x as usize).unwrap_or(0),
                evidence: ev,
                class,
            });
        }
        let power_devices: Vec<String> = extra_list("usb", "power_device")
            .into_iter()
            .filter(|name| present(mono, "usb", "power_device", name))
            .collect();
        if !classes.is_empty() || !power_devices.is_empty() {
            s.usb.push(crate::session::UsbPoint {
                t,
                t_start_ms: tms(mono, ID_USB_T_START),
                t_end_ms: tms(mono, ID_USB_T_END),
                classes,
                power_devices,
                power_error: None,
                changes: Vec::new(),
            });
        }
        if has(&[
            19,
            20,
            21,
            22,
            ID_SELF_PRIV_MB,
            ID_SELF_IO_READ,
            ID_SELF_CTX_SWITCHES,
        ]) {
            s.selfmon.push(crate::session::SelfPoint {
                t,
                t_start_ms: tms(mono, ID_SELF_T_START),
                t_end_ms: tms(mono, ID_SELF_T_END),
                cpu_pct: g(mono, 19),
                ws_mb: g(mono, 20),
                io_write_b: g(mono, 21),
                threads: g(mono, 22),
                priv_mb: g(mono, ID_SELF_PRIV_MB),
                io_read_b: g(mono, ID_SELF_IO_READ),
                ctx_switches: g(mono, ID_SELF_CTX_SWITCHES),
            });
        }
    }
    // Policy history + battery meta + footer + events.
    if let Some(list) = manifest.get("policy_history").and_then(|m| m.arr()) {
        for p in list {
            let t = p.get("t").and_then(|v| v.num()).unwrap_or(0.0);
            let stamp = ClockStamp {
                wall_millis: 0,
                mono_millis: (t * 1000.0) as u64,
            };
            let pt = |key: &str| match p.get(key) {
                Some(o) => crate::session::ev_inner(Some(o), "os", stamp),
                None => Telemetry::unavailable(
                    UnavailKind::NotSampled,
                    "policy field missing",
                    "os",
                    stamp,
                ),
            };
            let pol = crate::session::Policy {
                scheme_name: p
                    .get("scheme")
                    .and_then(|v| v.as_str())
                    .map(|x| x.to_string()),
                scheme_guid: p
                    .get("scheme_guid")
                    .and_then(|v| v.as_str())
                    .map(|x| x.to_string()),
                policy_snapshot_wall_ms: p
                    .get("snapshot_wall_ms")
                    .and_then(|v| v.num())
                    .map(|x| x as u64),
                timer_resolution: pt("timer_resolution"),
                cpu_min_ac: pt("cpu_min_ac"),
                cpu_max_ac: pt("cpu_max_ac"),
                cpu_min_dc: pt("cpu_min_dc"),
                cpu_max_dc: pt("cpu_max_dc"),
                epp_ac: pt("epp_ac"),
                epp_dc: pt("epp_dc"),
                brightness_ac: pt("brightness_ac"),
                brightness_dc: pt("brightness_dc"),
                display_timeout_ac: pt("display_timeout_ac"),
                display_timeout_dc: pt("display_timeout_dc"),
                sleep_timeout_ac: pt("sleep_timeout_ac"),
                sleep_timeout_dc: pt("sleep_timeout_dc"),
                hibernate_timeout_ac: pt("hibernate_timeout_ac"),
                hibernate_timeout_dc: pt("hibernate_timeout_dc"),
                low_batt_ac: pt("low_batt_ac"),
                low_batt_dc: pt("low_batt_dc"),
                crit_batt_ac: pt("crit_batt_ac"),
                crit_batt_dc: pt("crit_batt_dc"),
            };
            if s.policy.scheme_name.is_none() {
                s.policy = pol.clone();
            }
            s.policy_history.push((t, pol));
        }
    }
    if let Some(b) = manifest.get("battery") {
        let num = |k: &str| b.get(k).and_then(|v| v.num());
        // Static provenance strings: missing (older manifests) or unknown
        // means "unavailable" — never fabricated.
        let prov = |k: &str| match b.get(k).and_then(|v| v.as_str()) {
            Some("measured") | Some("derived") | Some("estimated") | Some("unavailable") => b
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("unavailable")
                .to_string(),
            _ => "unavailable".to_string(),
        };
        s.battery_meta.full_mwh = num("full_mwh");
        s.battery_meta.design_mwh = num("design_mwh");
        s.battery_meta.battery_index = num("index").map(|v| v as u32);
        s.battery_meta.battery_count = num("count").map(|v| v as u32);
        s.battery_meta.full_prov = prov("full_prov");
        s.battery_meta.design_prov = prov("design_prov");
        if let Some(arr) = b.get("batteries").and_then(|v| v.arr()) {
            let sopt = |o: &crate::json::JVal, k: &str| {
                o.get(k)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            };
            let eprov = |o: &crate::json::JVal, k: &str| match o.get(k).and_then(|v| v.as_str()) {
                Some("measured") | Some("derived") | Some("estimated") | Some("unavailable") => o
                    .get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or("unavailable")
                    .to_string(),
                _ => "unavailable".to_string(),
            };
            s.battery_meta.batteries = arr
                .iter()
                .map(|e| crate::session::BatteryStaticEntry {
                    index: e.get("index").and_then(|v| v.num()).unwrap_or(0.0) as u32,
                    designed_mwh: e.get("designed_mwh").and_then(|v| v.num()),
                    full_mwh: e.get("full_mwh").and_then(|v| v.num()),
                    cycle_count: e.get("cycle_count").and_then(|v| v.num()).map(|v| v as u32),
                    chemistry: sopt(e, "chemistry"),
                    device_name: sopt(e, "device_name"),
                    unique_id: sopt(e, "unique_id"),
                    technology: e.get("technology").and_then(|v| v.num()).map(|v| v as u32),
                    capabilities: e
                        .get("capabilities")
                        .and_then(|v| v.num())
                        .map(|v| v as u32),
                    temperature_raw: e
                        .get("temperature_raw")
                        .and_then(|v| v.num())
                        .map(|v| v as u32),
                    mfg_name: sopt(e, "mfg_name"),
                    mfg_date: sopt(e, "mfg_date"),
                    design_prov: eprov(e, "designed_prov"),
                    full_prov: eprov(e, "full_prov"),
                })
                .collect();
        }
    }
    // Manifest identity fields: accepted when missing (older archives);
    // verified-but-tolerated when present (never a hard failure).
    let _tool_version = manifest.get("tool_version").and_then(|v| v.as_str());
    let _machine_id = manifest.get("machine_id").and_then(|v| v.as_str());
    if let Some(want) = manifest.get("source_hash").and_then(|v| v.as_str()) {
        let got = source_hash_hex(&s.label, s.wall_base_ms, record_count);
        let _ = (want, got); // tolerate mismatch: warn-free, fail-free
    }
    for line in events {
        if let Ok(v) = parse(line) {
            s.events.push(crate::session::EventRec {
                wall_ms: v.get("wall_ms").and_then(|m| m.num()).unwrap_or(0.0) as u64,
                mono_ms: v.get("mono_ms").and_then(|m| m.num()).unwrap_or(0.0) as u64,
                kind: v
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("?")
                    .to_string(),
                detail: v
                    .get("detail")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    if let Some(footer) = manifest.get("footer")
        && !matches!(footer, JVal::Null)
    {
        s.footer = Some(footer.clone());
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        BatteryPoint, CpuPoint, DisplayPoint, GpuAdapterPoint, GpuPoint, NetPoint, ProcEntry,
        ProcPoint, StoragePoint, UsbPoint,
    };
    use crate::telemetry::{ClockStamp, Provenance, Telemetry, UnavailKind};

    fn ts(t: f64) -> ClockStamp {
        // Wall tracks mono here (wall_base 0): stamps must be per-tick or
        // the loader (which stamps from line times) can never match.
        ClockStamp {
            wall_millis: t as u64 * 1000,
            mono_millis: t as u64 * 1000,
        }
    }

    fn demo_session() -> SessionData {
        let mut s = SessionData {
            label: "demo".to_string(),
            ..Default::default()
        };
        for i in 0..5 {
            let t = i as f64;
            // Every field fully specified (no Default sources): the gate
            // compares sources, so fixtures must be realistic.
            s.battery.push(BatteryPoint {
                t,
                discharge: Telemetry::measured(6.0 + i as f64 * 0.1, "battery", ts(t)),
                charge: Telemetry::unavailable(
                    UnavailKind::NotSampled,
                    "not charging",
                    "battery",
                    ts(t),
                ),
                remaining_wh: Telemetry::measured(30.0, "battery", ts(t)),
                pct: Telemetry::measured(55.0, "battery", ts(t)),
            });
            s.battery_extra.push(crate::session::BatteryExtraPoint {
                t,
                ac: Telemetry::measured(0.0, "battery", ts(t)),
                rate_mw: Telemetry::measured(-5700.0, "battery", ts(t)),
                health: Telemetry::derived(0.92, "battery", ts(t)),
                temperature_raw: Telemetry::measured(2982.0, "battery", ts(t)),
                t_start_ms: Some(10),
                t_end_ms: Some(90),
            });
            s.cpu.push(CpuPoint {
                t,
                t_start_ms: Some(10),
                t_end_ms: Some(90),
                utility: Telemetry::measured(5.0, "cpu", ts(t)),
                c3_pct: Telemetry::measured(90.0, "cpu", ts(t)),
                idle_breaks: Telemetry::measured(1000.0, "cpu", ts(t)),
                pkg_derived_w: Telemetry::estimated(2.0, "cpu", ts(t)),
                pkg_power_w: Telemetry::estimated(2.1, "cpu", ts(t)),
                ctx_switches: Telemetry::measured(5000.0, "cpu", ts(t)),
                cores: vec![
                    crate::session::CpuCorePoint {
                        instance: "0,0".to_string(),
                        utility: Telemetry::measured(4.0, "cpu", ts(t)),
                        performance: Telemetry::measured(110.0, "cpu", ts(t)),
                        freq_mhz: Telemetry::measured(2300.0, "cpu", ts(t)),
                        parking: Telemetry::measured(0.0, "cpu", ts(t)),
                    },
                    crate::session::CpuCorePoint {
                        instance: "0,1".to_string(),
                        utility: Telemetry::measured(6.0, "cpu", ts(t)),
                        performance: Telemetry::measured(120.0, "cpu", ts(t)),
                        freq_mhz: Telemetry::measured(2400.0, "cpu", ts(t)),
                        parking: Telemetry::measured(0.0, "cpu", ts(t)),
                    },
                ],
                totals: vec![
                    (
                        "performance".to_string(),
                        Telemetry::measured(115.0, "cpu", ts(t)),
                    ),
                    (
                        "idle_pct".to_string(),
                        Telemetry::measured(80.0, "cpu", ts(t)),
                    ),
                    (
                        "c1_trans".to_string(),
                        Telemetry::measured(300.0, "cpu", ts(t)),
                    ),
                ],
            });
            s.gpu.push(GpuPoint {
                t,
                t_start_ms: Some(5),
                t_end_ms: Some(95),
                stale: false,
                adapters: vec![GpuAdapterPoint {
                    name: "AMD".to_string(),
                    discrete: false,
                    total_util: Telemetry::derived(1.5, "gpu", ts(t)),
                    mem_dedicated_mb: Telemetry::derived(300.0, "gpu", ts(t)),
                    mem_shared_mb: Telemetry::derived(50.0, "gpu", ts(t)),
                    util_3d: Telemetry::derived(1.0, "gpu", ts(t)),
                    util_compute: Telemetry::derived(0.2, "gpu", ts(t)),
                    util_decode: Telemetry::derived(0.3, "gpu", ts(t)),
                    util_codec: Telemetry::derived(0.0, "gpu", ts(t)),
                    util_copy: Telemetry::derived(0.0, "gpu", ts(t)),
                    util_other: Telemetry::derived(0.0, "gpu", ts(t)),
                    engines_active: Telemetry::derived(1.0, "gpu", ts(t)),
                    engines_seen: Telemetry::derived(3.0, "gpu", ts(t)),
                    awake: Some(crate::session::AwakeRec {
                        active: true,
                        confidence: "high".to_string(),
                        evidence: "engine activity".to_string(),
                    }),
                    top_pids: vec![(200, Telemetry::derived(1.2, "gpu", ts(t)))],
                }],
            });
            s.procs.push(ProcPoint {
                t,
                t_start_ms: Some(5),
                t_end_ms: Some(95),
                total_threads: Some(400),
                inaccessible: Some(2),
                truncated: Some(true),
                top: vec![ProcEntry {
                    pid: 100,
                    ppid: 1,
                    name: "a.exe".to_string(),
                    start_unix_ms: Some(999),
                    exit_unix_ms: Some(4321),
                    cpu: Telemetry::derived(0.5, "proc", ts(t)),
                }],
            });
            s.display.push(DisplayPoint {
                t,
                t_start_ms: Some(5),
                t_end_ms: Some(95),
                brightness: Telemetry::measured(40.0, "display", ts(t)),
                displays: vec![
                    crate::session::DisplayEntry {
                        name: "PANEL".to_string(),
                        primary: true,
                        width: Some(1920),
                        height: Some(1200),
                        freq_hz: Some(60),
                        bpp: Some(32),
                        brightness: Telemetry::measured(40.0, "display", ts(t)),
                    },
                    crate::session::DisplayEntry {
                        name: "EXT".to_string(),
                        primary: false,
                        width: Some(2560),
                        height: Some(1440),
                        freq_hz: Some(144),
                        bpp: Some(32),
                        brightness: Telemetry::measured(30.0, "display", ts(t)),
                    },
                ],
                unmatched_sensors: vec![("DISPLAY\\ZZZ".to_string(), Some(20.0))],
                hdr_targets: vec![crate::session::HdrTargetRec {
                    adapter: "1/0".to_string(),
                    target: 0,
                    supported: true,
                    enabled: false,
                }],
                hdr_error: None,
                changes: Vec::new(),
            });
            s.net.push(NetPoint {
                t,
                t_start_ms: Some(5),
                t_end_ms: Some(95),
                total_rx: Telemetry::derived(100.0, "net", ts(t)),
                total_tx: Telemetry::derived(50.0, "net", ts(t)),
                adapters: vec![crate::session::NetAdapterRec {
                    alias: "WiFi".to_string(),
                    descr: "MediaTek".to_string(),
                    class: "wifi".to_string(),
                    iftype: "wifi".to_string(),
                    oper: Some(1.0),
                    media: Some(1.0),
                    admin_up: true,
                    tx_mbps: Telemetry::measured(100.0, "net", ts(t)),
                    rx_mbps: Telemetry::measured(100.0, "net", ts(t)),
                    in_octets: Telemetry::measured(1000.0, "net", ts(t)),
                    out_octets: Telemetry::measured(2000.0, "net", ts(t)),
                }],
                throughput: vec![(
                    "WiFi".to_string(),
                    Telemetry::measured(100.0, "net", ts(t)),
                    Telemetry::measured(50.0, "net", ts(t)),
                )],
                wifi: vec![crate::session::WifiRec {
                    descr: "MediaTek".to_string(),
                    state: Some(1.0),
                    ssid: "Home".to_string(),
                    signal_pct: Telemetry::measured(80.0, "net", ts(t)),
                }],
                changes: Vec::new(),
                topology_stale: false,
            });
            s.storage.push(StoragePoint {
                t,
                t_start_ms: Some(5),
                t_end_ms: Some(95),
                disk_time: Telemetry::measured(2.0, "storage", ts(t)),
                read_bps: Telemetry::measured(1000.0, "storage", ts(t)),
                write_bps: Telemetry::measured(2000.0, "storage", ts(t)),
            });
            s.usb.push(UsbPoint {
                t,
                t_start_ms: Some(5),
                t_end_ms: Some(95),
                classes: vec![
                    crate::session::UsbClassCount {
                        class: "usb".to_string(),
                        count: 6,
                        evidence: Telemetry::measured(6.0, "usb", ts(t)),
                    },
                    crate::session::UsbClassCount {
                        class: "hid".to_string(),
                        count: 0,
                        evidence: Telemetry::unavailable(
                            UnavailKind::NotSampled,
                            "no hid devices",
                            "usb",
                            ts(t),
                        ),
                    },
                ],
                power_devices: vec!["SPPSERVICE4".to_string()],
                power_error: None,
                changes: Vec::new(),
            });
            s.selfmon.push(crate::session::SelfPoint {
                t,
                t_start_ms: Some(5),
                t_end_ms: Some(95),
                cpu_pct: Telemetry::measured(1.5, "self", ts(t)),
                ws_mb: Telemetry::measured(40.0, "self", ts(t)),
                priv_mb: Telemetry::measured(30.0, "self", ts(t)),
                io_read_b: Telemetry::derived(4096.0, "self", ts(t)),
                io_write_b: Telemetry::derived(8192.0, "self", ts(t)),
                threads: Telemetry::measured(9.0, "self", ts(t)),
                ctx_switches: Telemetry::measured(5000.0, "self", ts(t)),
            });
        }
        s.battery_meta = crate::session::BatteryMeta {
            full_mwh: Some(38700.0),
            design_mwh: Some(42000.0),
            battery_index: Some(0),
            battery_count: Some(2),
            full_prov: "measured".to_string(),
            design_prov: "estimated".to_string(),
            batteries: vec![
                crate::session::BatteryStaticEntry {
                    index: 0,
                    designed_mwh: Some(42000.0),
                    full_mwh: Some(38700.0),
                    cycle_count: Some(123),
                    chemistry: Some("LION".to_string()),
                    device_name: Some("BAT0".to_string()),
                    unique_id: Some("UID0".to_string()),
                    technology: Some(1),
                    capabilities: Some(0x1),
                    temperature_raw: Some(2982),
                    mfg_name: Some("ASUS".to_string()),
                    mfg_date: Some("2024-01-02".to_string()),
                    design_prov: "measured".to_string(),
                    full_prov: "measured".to_string(),
                },
                crate::session::BatteryStaticEntry {
                    index: 1,
                    designed_mwh: Some(20000.0),
                    full_mwh: Some(19000.0),
                    cycle_count: Some(50),
                    chemistry: Some("LION".to_string()),
                    device_name: Some("BAT1".to_string()),
                    unique_id: None,
                    technology: None,
                    capabilities: None,
                    temperature_raw: None,
                    mfg_name: None,
                    mfg_date: None,
                    design_prov: "estimated".to_string(),
                    full_prov: "unavailable".to_string(),
                },
            ],
        };
        s.events.push(crate::session::EventRec {
            wall_ms: 1,
            mono_ms: 0,
            kind: "k".to_string(),
            detail: "d".to_string(),
        });
        s
    }

    #[test]
    fn roundtrip_preserves_series() {
        let s = demo_session();
        let a = migrate("demo", "demo.jsonl", &s);
        // v2 records are variable-length (reasons inline); count comes
        // from the migrator, not division.
        assert!(a.records > 60);
        assert!(a.manifest.contains("pf-archive-2"));
        assert!(a.manifest.contains("\"schema\":1"));
        assert!(a.manifest.contains("a.exe"));
        // Little-endian lock-in: first record opens with mono_ms LE.
        assert_eq!(&a.samples[0..8], &0u64.to_le_bytes());
        assert_eq!(&a.samples[16..18], &0u16.to_le_bytes());
        let back = load_archive_v2("demo", &a.manifest, &a.samples, &a.events).unwrap();
        assert_session_eq(&s, &back);
    }

    /// Analytical equality: every value, provenance, kind, reason,
    /// quality, timestamp, identity table, policy history row, battery
    /// metadata field, and event must match.
    fn assert_session_eq(a: &SessionData, b: &SessionData) {
        assert_eq!(a.label, b.label);
        assert_eq!(a.wall_base_ms, b.wall_base_ms);
        let teq = |x: &Telemetry<f64>, y: &Telemetry<f64>| {
            assert_eq!(x.val(), y.val(), "value");
            assert_eq!(x.provenance(), y.provenance(), "provenance");
            assert_eq!(x.unavail_kind, y.unavail_kind, "kind");
            assert_eq!(x.quality, y.quality, "quality");
            assert_eq!(x.source, y.source, "source");
            assert_eq!(x.stamp, y.stamp, "stamp");
            if x.val().is_none() {
                assert_eq!(x.value.reason(), y.value.reason(), "reason");
            }
        };
        assert_eq!(a.battery.len(), b.battery.len());
        for (x, y) in a.battery.iter().zip(&b.battery) {
            assert!((x.t - y.t).abs() < 1e-9);
            teq(&x.discharge, &y.discharge);
            teq(&x.charge, &y.charge);
            teq(&x.remaining_wh, &y.remaining_wh);
            teq(&x.pct, &y.pct);
        }
        assert_eq!(a.battery_extra.len(), b.battery_extra.len());
        for (x, y) in a.battery_extra.iter().zip(&b.battery_extra) {
            assert!((x.t - y.t).abs() < 1e-9);
            teq(&x.ac, &y.ac);
            teq(&x.rate_mw, &y.rate_mw);
            teq(&x.health, &y.health);
            teq(&x.temperature_raw, &y.temperature_raw);
            assert_eq!((x.t_start_ms, x.t_end_ms), (y.t_start_ms, y.t_end_ms));
        }
        assert_eq!(a.cpu.len(), b.cpu.len());
        for (x, y) in a.cpu.iter().zip(&b.cpu) {
            teq(&x.utility, &y.utility);
            teq(&x.c3_pct, &y.c3_pct);
            teq(&x.idle_breaks, &y.idle_breaks);
            teq(&x.pkg_derived_w, &y.pkg_derived_w);
            teq(&x.pkg_power_w, &y.pkg_power_w);
            teq(&x.ctx_switches, &y.ctx_switches);
            assert_eq!((x.t_start_ms, x.t_end_ms), (y.t_start_ms, y.t_end_ms));
            assert_eq!(x.cores.len(), y.cores.len());
            for (xc, yc) in x.cores.iter().zip(&y.cores) {
                assert_eq!(xc.instance, yc.instance);
                teq(&xc.utility, &yc.utility);
                teq(&xc.performance, &yc.performance);
                teq(&xc.freq_mhz, &yc.freq_mhz);
                teq(&xc.parking, &yc.parking);
            }
            assert_eq!(x.totals.len(), y.totals.len());
            for ((k1, v1), (k2, v2)) in x.totals.iter().zip(&y.totals) {
                assert_eq!(k1, k2);
                teq(v1, v2);
            }
        }
        assert_eq!(a.gpu.len(), b.gpu.len());
        for (x, y) in a.gpu.iter().zip(&b.gpu) {
            assert_eq!(x.stale, y.stale);
            assert_eq!((x.t_start_ms, x.t_end_ms), (y.t_start_ms, y.t_end_ms));
            assert_eq!(x.adapters.len(), y.adapters.len());
            for (xa, ya) in x.adapters.iter().zip(&y.adapters) {
                assert_eq!(xa.name, ya.name);
                teq(&xa.total_util, &ya.total_util);
                teq(&xa.mem_dedicated_mb, &ya.mem_dedicated_mb);
                teq(&xa.mem_shared_mb, &ya.mem_shared_mb);
                teq(&xa.util_3d, &ya.util_3d);
                teq(&xa.util_compute, &ya.util_compute);
                teq(&xa.util_decode, &ya.util_decode);
                teq(&xa.util_codec, &ya.util_codec);
                teq(&xa.util_copy, &ya.util_copy);
                teq(&xa.util_other, &ya.util_other);
                teq(&xa.engines_active, &ya.engines_active);
                teq(&xa.engines_seen, &ya.engines_seen);
                // Awake confidence/evidence are presentation-only.
                assert_eq!(
                    xa.awake.as_ref().map(|w| w.active),
                    ya.awake.as_ref().map(|w| w.active)
                );
                assert_eq!(xa.top_pids.len(), ya.top_pids.len());
                for ((p1, u1), (p2, u2)) in xa.top_pids.iter().zip(&ya.top_pids) {
                    assert_eq!(p1, p2);
                    teq(u1, u2);
                }
            }
        }
        assert_eq!(a.procs.len(), b.procs.len());
        for (x, y) in a.procs.iter().zip(&b.procs) {
            assert_eq!((x.t_start_ms, x.t_end_ms), (y.t_start_ms, y.t_end_ms));
            assert_eq!(
                (x.total_threads, x.inaccessible, x.truncated),
                (y.total_threads, y.inaccessible, y.truncated),
                "process coverage evidence must survive archive round-trip"
            );
            assert_eq!(x.top.len(), y.top.len());
            for (xe, ye) in x.top.iter().zip(&y.top) {
                assert_eq!(
                    (xe.pid, &xe.name, xe.ppid, xe.start_unix_ms, xe.exit_unix_ms),
                    (ye.pid, &ye.name, ye.ppid, ye.start_unix_ms, ye.exit_unix_ms)
                );
                teq(&xe.cpu, &ye.cpu);
            }
        }
        assert_eq!(a.display.len(), b.display.len());
        for (x, y) in a.display.iter().zip(&b.display) {
            assert_eq!((x.t_start_ms, x.t_end_ms), (y.t_start_ms, y.t_end_ms));
            teq(&x.brightness, &y.brightness);
            assert_eq!(x.displays.len(), y.displays.len());
            for (xd, yd) in x.displays.iter().zip(&y.displays) {
                assert_eq!(
                    (
                        &xd.name, xd.primary, xd.width, xd.height, xd.freq_hz, xd.bpp
                    ),
                    (
                        &yd.name, yd.primary, yd.width, yd.height, yd.freq_hz, yd.bpp
                    )
                );
                teq(&xd.brightness, &yd.brightness);
            }
            assert_eq!(x.unmatched_sensors, y.unmatched_sensors);
            assert_eq!(x.hdr_targets.len(), y.hdr_targets.len());
            for (xh, yh) in x.hdr_targets.iter().zip(&y.hdr_targets) {
                assert_eq!(
                    (&xh.adapter, xh.target, xh.supported, xh.enabled),
                    (&yh.adapter, yh.target, yh.supported, yh.enabled)
                );
            }
        }
        assert_eq!(a.net.len(), b.net.len());
        for (x, y) in a.net.iter().zip(&b.net) {
            assert_eq!((x.t_start_ms, x.t_end_ms), (y.t_start_ms, y.t_end_ms));
            teq(&x.total_rx, &y.total_rx);
            teq(&x.total_tx, &y.total_tx);
            assert_eq!(
                x.topology_stale, y.topology_stale,
                "net topology staleness must survive archive round-trip"
            );
            assert_eq!(x.adapters.len(), y.adapters.len());
            for (xd, yd) in x.adapters.iter().zip(&y.adapters) {
                assert_eq!(
                    (
                        &xd.alias,
                        &xd.descr,
                        &xd.class,
                        &xd.iftype,
                        xd.oper,
                        xd.media,
                        xd.admin_up
                    ),
                    (
                        &yd.alias,
                        &yd.descr,
                        &yd.class,
                        &yd.iftype,
                        yd.oper,
                        yd.media,
                        yd.admin_up
                    )
                );
                teq(&xd.tx_mbps, &yd.tx_mbps);
                teq(&xd.rx_mbps, &yd.rx_mbps);
                teq(&xd.in_octets, &yd.in_octets);
                teq(&xd.out_octets, &yd.out_octets);
            }
            assert_eq!(x.throughput.len(), y.throughput.len());
            for ((i1, r1, t1), (i2, r2, t2)) in x.throughput.iter().zip(&y.throughput) {
                assert_eq!(i1, i2);
                teq(r1, r2);
                teq(t1, t2);
            }
            assert_eq!(x.wifi.len(), y.wifi.len());
            for (xw, yw) in x.wifi.iter().zip(&y.wifi) {
                assert_eq!(
                    (&xw.descr, &xw.ssid, xw.state),
                    (&yw.descr, &yw.ssid, yw.state)
                );
                teq(&xw.signal_pct, &yw.signal_pct);
            }
        }
        assert_eq!(a.storage.len(), b.storage.len());
        for (x, y) in a.storage.iter().zip(&b.storage) {
            assert_eq!((x.t_start_ms, x.t_end_ms), (y.t_start_ms, y.t_end_ms));
            teq(&x.disk_time, &y.disk_time);
            teq(&x.read_bps, &y.read_bps);
            teq(&x.write_bps, &y.write_bps);
        }
        assert_eq!(a.usb.len(), b.usb.len());
        for (x, y) in a.usb.iter().zip(&b.usb) {
            assert!((x.t - y.t).abs() < 1e-9);
            assert_eq!((x.t_start_ms, x.t_end_ms), (y.t_start_ms, y.t_end_ms));
            assert_eq!(x.classes.len(), y.classes.len());
            for (xc, yc) in x.classes.iter().zip(&y.classes) {
                assert_eq!((&xc.class, xc.count), (&yc.class, yc.count));
                teq(&xc.evidence, &yc.evidence);
                if xc.evidence.val().is_none() {
                    assert_eq!(xc.evidence.value.reason(), yc.evidence.value.reason());
                }
            }
            assert_eq!(x.power_devices, y.power_devices);
        }
        assert_eq!(a.selfmon.len(), b.selfmon.len());
        for (x, y) in a.selfmon.iter().zip(&b.selfmon) {
            assert!((x.t - y.t).abs() < 1e-9);
            assert_eq!((x.t_start_ms, x.t_end_ms), (y.t_start_ms, y.t_end_ms));
            teq(&x.cpu_pct, &y.cpu_pct);
            teq(&x.ws_mb, &y.ws_mb);
            teq(&x.priv_mb, &y.priv_mb);
            teq(&x.io_read_b, &y.io_read_b);
            teq(&x.io_write_b, &y.io_write_b);
            teq(&x.threads, &y.threads);
            teq(&x.ctx_switches, &y.ctx_switches);
        }
        // Policy history (headline policy derived from first row).
        assert_eq!(a.policy_history.len(), b.policy_history.len());
        for ((ta, pa), (tb, pb)) in a.policy_history.iter().zip(&b.policy_history) {
            assert!((ta - tb).abs() < 1e-9);
            assert_eq!(pa.scheme_name, pb.scheme_name);
            assert_eq!(pa.scheme_guid, pb.scheme_guid);
            assert_eq!(pa.policy_snapshot_wall_ms, pb.policy_snapshot_wall_ms);
            teq(&pa.timer_resolution, &pb.timer_resolution);
            teq(&pa.cpu_min_ac, &pb.cpu_min_ac);
            teq(&pa.cpu_min_dc, &pb.cpu_min_dc);
            teq(&pa.cpu_max_ac, &pb.cpu_max_ac);
            teq(&pa.cpu_max_dc, &pb.cpu_max_dc);
            teq(&pa.epp_ac, &pb.epp_ac);
            teq(&pa.epp_dc, &pb.epp_dc);
            teq(&pa.brightness_ac, &pb.brightness_ac);
            teq(&pa.brightness_dc, &pb.brightness_dc);
            teq(&pa.display_timeout_ac, &pb.display_timeout_ac);
            teq(&pa.display_timeout_dc, &pb.display_timeout_dc);
            teq(&pa.sleep_timeout_ac, &pb.sleep_timeout_ac);
            teq(&pa.sleep_timeout_dc, &pb.sleep_timeout_dc);
            teq(&pa.hibernate_timeout_ac, &pb.hibernate_timeout_ac);
            teq(&pa.hibernate_timeout_dc, &pb.hibernate_timeout_dc);
            teq(&pa.low_batt_ac, &pb.low_batt_ac);
            teq(&pa.low_batt_dc, &pb.low_batt_dc);
            teq(&pa.crit_batt_ac, &pb.crit_batt_ac);
            teq(&pa.crit_batt_dc, &pb.crit_batt_dc);
        }
        assert_eq!(a.battery_meta.full_mwh, b.battery_meta.full_mwh);
        assert_eq!(a.battery_meta.design_mwh, b.battery_meta.design_mwh);
        assert_eq!(a.battery_meta.battery_index, b.battery_meta.battery_index);
        assert_eq!(a.battery_meta.battery_count, b.battery_meta.battery_count);
        assert_eq!(a.battery_meta.full_prov, b.battery_meta.full_prov);
        assert_eq!(a.battery_meta.design_prov, b.battery_meta.design_prov);
        assert_eq!(a.battery_meta.batteries, b.battery_meta.batteries);
        assert_eq!(a.events.len(), b.events.len());
        for (x, y) in a.events.iter().zip(&b.events) {
            assert_eq!(
                (x.wall_ms, x.mono_ms, &x.kind, &x.detail),
                (y.wall_ms, y.mono_ms, &y.kind, &y.detail)
            );
        }
    }

    #[test]
    fn v2_unknown_prov_does_not_become_measured() {
        // Hand-crafted record with prov=9 (undefined): must load as
        // Unavailable, never as Measured.
        let s = demo_session();
        let a = migrate("demo", "demo.jsonl", &s);
        let mut bad = a.samples.clone();
        // First record's prov byte sits at offset 18.
        bad[18] = 9;
        let back = load_archive_v2("demo", &a.manifest, &bad, &a.events).unwrap();
        assert_eq!(back.battery[0].discharge.val(), None);
        assert_eq!(
            back.battery[0].discharge.provenance(),
            Provenance::Unavailable
        );
    }

    #[test]
    fn loader_rejects_garbage() {
        assert!(load_archive("x", "not json", &[], &[]).is_err());
        assert!(load_archive("x", "{\"format\":\"nope\"}", &[], &[]).is_err());
        assert!(
            load_archive(
                "x",
                &migrate("m", "s", &SessionData::default()).manifest,
                &[1u8; 7],
                &[]
            )
            .is_err()
        );
        assert!(load_archive_v2("x", "not json", &[], &[]).is_err());
        assert!(load_archive_v2("x", "{\"format\":\"pf-archive-1\"}", &[], &[]).is_err());
    }

    #[test]
    fn prov_codec() {
        assert_eq!(prov_byte("measured"), 0);
        assert_eq!(prov_byte("derived"), 1);
        assert_eq!(prov_byte("estimated"), 2);
        assert_eq!(prov_byte("unavailable"), 3);
        // Unknown strings never become measured.
        assert_eq!(prov_byte("mystery"), 3);
        assert_eq!(prov_byte(""), 3);
        assert_eq!(prov_str(0), "measured");
        assert_eq!(prov_str(1), "derived");
        assert_eq!(prov_str(2), "estimated");
        assert_eq!(prov_str(3), "unavailable");
        assert_eq!(prov_str(9), "unavailable");
    }

    #[test]
    fn usb_provenance_round_trip() {
        let s = demo_session();
        let a = migrate("demo", "demo.jsonl", &s);
        let back = load_archive_v2("demo", &a.manifest, &a.samples, &a.events).unwrap();
        assert_eq!(back.usb.len(), s.usb.len());
        // Measured class keeps prov/quality/source/stamp.
        let hid0 = &back.usb[0].classes[1];
        assert_eq!(hid0.class, "hid");
        assert_eq!(hid0.count, 0);
        assert_eq!(hid0.evidence.val(), None);
        assert_eq!(hid0.evidence.provenance(), Provenance::Unavailable);
        assert_eq!(hid0.evidence.value.reason(), Some("no hid devices"));
        let usb0 = &back.usb[0].classes[0];
        assert_eq!(usb0.evidence.val(), Some(6.0));
        assert_eq!(usb0.evidence.provenance(), Provenance::Measured);
        assert_eq!(usb0.evidence.source, "usb");
    }

    #[test]
    fn selfmon_round_trip() {
        let s = demo_session();
        let a = migrate("demo", "demo.jsonl", &s);
        assert!(a.manifest.contains("\"metric\":\"threads\""));
        let back = load_archive_v2("demo", &a.manifest, &a.samples, &a.events).unwrap();
        assert_eq!(back.selfmon.len(), 5);
        assert_eq!(back.selfmon[0].cpu_pct.val(), Some(1.5));
        assert_eq!(back.selfmon[0].io_write_b.provenance(), Provenance::Derived);
        assert_eq!(back.selfmon[0].threads.val(), Some(9.0));
        assert_eq!(back.selfmon[0].threads.source, "self");
    }

    #[test]
    fn manifest_fields_present_and_backward_compat() {
        let s = demo_session();
        let a = migrate("demo", "demo.jsonl", &s);
        assert!(a.manifest.contains("\"tool_version\":\"0.1.0\""));
        assert!(a.manifest.contains("\"machine_id\":\""));
        assert!(a.manifest.contains("\"source_hash\":\""));
        assert!(a.manifest.contains("\"exit_unix_ms\":4321"));
        assert!(a.manifest.contains("\"full_prov\":\"measured\""));
        // Missing identity fields stay loadable (older manifests).
        let stripped = a.manifest.replace("\"tool_version\":\"0.1.0\",", "");
        let back = load_archive_v2("demo", &stripped, &a.samples, &a.events).unwrap();
        assert_eq!(back.selfmon.len(), 5);
        // Wrong hash never hard-fails.
        let wrong = a
            .manifest
            .replace("\"source_hash\":\"", "\"source_hash\":\"ffff");
        let back2 = load_archive_v2("demo", &wrong, &a.samples, &a.events).unwrap();
        assert_eq!(back2.selfmon.len(), 5);
    }

    /// Phase-3 gate, part 2: diagnosis and comparison run on the archived
    /// representation must produce identical output to the JSONL path.
    /// No forensic conclusion may change merely because a session was archived.
    #[test]
    fn analysis_equivalent_across_representations() {
        use crate::session::load_session;
        let mut lines = vec![
            "{\"type\":\"session_header\",\"wall_ms\":1000,\"tool\":\"t\",\"version\":\"0.1.0\",\"format\":\"pf-jsonl-2\",\"interval_ms\":1000,\"note\":\"gate\"}"
                .to_string(),
        ];
        for i in 0..24 {
            let hi = i >= 12;
            let t = i * 1000;
            let w = 1000 + t;
            let dw = if hi { 11.4 } else { 5.7 };
            lines.push(format!(
                "{{\"collector\":\"battery\",\"wall_ms\":{w},\"mono_ms\":{t},\
                 \"discharge_w\":{{\"v\":{dw},\"p\":\"measured\",\"q\":0}},\
                 \"remaining_mwh\":{{\"v\":29000,\"p\":\"measured\",\"q\":0}}}}"
            ));
            let (u, c3, pkg) = if hi {
                (18.0, 40.0, 4.2)
            } else {
                (4.0, 92.0, 1.1)
            };
            lines.push(format!(
                "{{\"collector\":\"cpu\",\"wall_ms\":{w},\"mono_ms\":{t},\
                 \"totals\":{{\"utility\":{{\"v\":{u},\"p\":\"measured\",\"q\":0}},\
                 \"c3_pct\":{{\"v\":{c3},\"p\":\"measured\",\"q\":0}}}},\
                 \"energy\":{{\"pkg_derived_w\":{{\"v\":{pkg},\"p\":\"estimated\",\"q\":0}}}}}}"
            ));
            let gu = if hi { 6.0 } else { 0.0 };
            lines.push(format!(
                "{{\"collector\":\"gpu\",\"wall_ms\":{w},\"mono_ms\":{t},\"stale\":false,\
                 \"adapters\":[{{\"name\":\"dGPU\",\"discrete\":true,\"total_util_pct\":{{\"v\":{gu},\"p\":\"derived\",\"q\":0}}}}]}}"
            ));
            let procs = if hi {
                "[{\"pid\":100,\"ppid\":1,\"name\":\"idle.exe\",\"cpu_pct\":{\"v\":0.1,\"p\":\"derived\",\"q\":0}},\
                 {\"pid\":200,\"ppid\":1,\"name\":\"game.exe\",\"start_unix_ms\":2000,\"cpu_pct\":{\"v\":12.0,\"p\":\"derived\",\"q\":0}}]"
            } else {
                "[{\"pid\":100,\"ppid\":1,\"name\":\"idle.exe\",\"cpu_pct\":{\"v\":0.1,\"p\":\"derived\",\"q\":0}}]"
            };
            lines.push(format!(
                "{{\"collector\":\"proc\",\"wall_ms\":{w},\"mono_ms\":{t},\"total_procs\":150,\"top\":{procs}}}"
            ));
            lines.push(format!(
                "{{\"collector\":\"display\",\"wall_ms\":{w},\"mono_ms\":{t},\
                 \"displays\":[{{\"name\":\"PANEL\",\"brightness_pct\":{{\"v\":40,\"p\":\"measured\",\"q\":0}}}}]}}"
            ));
        }
        lines.push("{\"type\":\"session_footer\",\"wall_ms\":25000,\"lines\":999,\"summary\":{\"discharge_wh\":0.05}}".to_string());
        let text = lines.join("\n");
        let s = load_session("gate", &text).unwrap();
        assert_eq!(s.battery.len(), 24);
        let a = migrate("gate", "gate.jsonl", &s);
        let back = load_archive_v2("gate", &a.manifest, &a.samples, &a.events).unwrap();
        assert_session_eq(&s, &back);
        let d1 = crate::analysis::diagnose_session(&s, None);
        let d2 = crate::analysis::diagnose_session(&back, None);
        assert!(d1.step.is_some() && d2.step.is_some());
        assert_eq!(
            crate::analysis::render_diagnosis("g", &d1),
            crate::analysis::render_diagnosis("g", &d2)
        );
        let c1 = crate::analysis::compare_sessions(&s, &s);
        let c2 = crate::analysis::compare_sessions(&back, &back);
        assert_eq!(
            crate::analysis::render_comparison(&c1),
            crate::analysis::render_comparison(&c2)
        );
        let r1 = crate::analysis::render_report(&s, &d1);
        let r2 = crate::analysis::render_report(&back, &d2);
        assert_eq!(r1, r2);
    }

    #[test]
    fn duplicate_metric_ids_are_rejected_not_last_wins() {
        let manifest = "{\"format\":\"pf-archive-2\",\"schema\":1,\"metrics\":[\
            {\"id\":1000,\"collector\":\"battery\",\"metric\":\"discharge_w\",\"unit\":\"W\",\"extra\":\"\"},\
            {\"id\":1000,\"collector\":\"battery\",\"metric\":\"charge_w\",\"unit\":\"W\",\"extra\":\"\"}],\
            \"wall_base_ms\":0}";
        let e = load_archive_v2("d", manifest, &[], &[]).unwrap_err();
        assert!(e.contains("duplicate metric id 1000"), "{e}");
        // The v1 reader applies the same deterministic rule.
        let v1 = manifest.replace("pf-archive-2", "pf-archive-1");
        let e1 = load_archive("d", &v1, &[], &[]).unwrap_err();
        assert!(e1.contains("duplicate metric id 1000"), "{e1}");
    }

    /// Presentation absence must survive an archive round-trip: a `None`
    /// mode dimension or adapter state must never come back as `Some(0)`.
    #[test]
    fn presentation_absence_survives_archive_round_trip() {
        let t = 0.0;
        let mut s = SessionData {
            label: "pres".to_string(),
            ..Default::default()
        };
        s.display.push(DisplayPoint {
            t,
            brightness: Telemetry::measured(40.0, "display", ts(t)),
            displays: vec![
                crate::session::DisplayEntry {
                    name: "NO_MODE".to_string(),
                    primary: true,
                    width: None,
                    height: None,
                    freq_hz: None,
                    bpp: None,
                    brightness: Telemetry::measured(40.0, "display", ts(t)),
                },
                crate::session::DisplayEntry {
                    name: "REPORTED".to_string(),
                    primary: false,
                    width: Some(1920),
                    height: Some(0),
                    freq_hz: Some(60),
                    bpp: Some(32),
                    brightness: Telemetry::measured(30.0, "display", ts(t)),
                },
            ],
            ..Default::default()
        });
        s.net.push(NetPoint {
            t,
            total_rx: Telemetry::derived(1.0, "net", ts(t)),
            adapters: vec![
                crate::session::NetAdapterRec {
                    alias: "A".to_string(),
                    descr: "d".to_string(),
                    class: "wifi".to_string(),
                    iftype: "wifi".to_string(),
                    oper: None,
                    media: None,
                    admin_up: false,
                    tx_mbps: Telemetry::measured(1.0, "net", ts(t)),
                    rx_mbps: Telemetry::measured(1.0, "net", ts(t)),
                    in_octets: Telemetry::measured(1.0, "net", ts(t)),
                    out_octets: Telemetry::measured(1.0, "net", ts(t)),
                },
                crate::session::NetAdapterRec {
                    alias: "B".to_string(),
                    descr: "d".to_string(),
                    class: "wifi".to_string(),
                    iftype: "wifi".to_string(),
                    oper: Some(0.0),
                    media: Some(1.0),
                    admin_up: true,
                    tx_mbps: Telemetry::measured(1.0, "net", ts(t)),
                    rx_mbps: Telemetry::measured(1.0, "net", ts(t)),
                    in_octets: Telemetry::measured(1.0, "net", ts(t)),
                    out_octets: Telemetry::measured(1.0, "net", ts(t)),
                },
            ],
            wifi: vec![crate::session::WifiRec {
                descr: "w".to_string(),
                state: None,
                ssid: "s".to_string(),
                signal_pct: Telemetry::measured(80.0, "net", ts(t)),
            }],
            ..Default::default()
        });
        let a = migrate("pres", "p.jsonl", &s);
        let back = load_archive_v2("pres", &a.manifest, &a.samples, &a.events).unwrap();
        let d = &back.display[0].displays;
        assert_eq!(
            (d[0].width, d[0].height, d[0].freq_hz, d[0].bpp),
            (None, None, None, None),
            "absent mode dimensions must stay absent"
        );
        assert_eq!(
            (d[1].width, d[1].height, d[1].freq_hz, d[1].bpp),
            (Some(1920), Some(0), Some(60), Some(32)),
            "reported values (including a genuine 0) must survive"
        );
        let n = &back.net[0].adapters;
        assert_eq!((n[0].oper, n[0].media), (None, None));
        assert_eq!((n[1].oper, n[1].media), (Some(0.0), Some(1.0)));
        assert_eq!(back.net[0].wifi[0].state, None);
    }

    fn v1_record(mono: u64, id: u16, prov: u8, val: f64) -> Vec<u8> {
        let mut b = Vec::with_capacity(RECORD_LEN);
        b.extend_from_slice(&mono.to_le_bytes());
        b.extend_from_slice(&id.to_le_bytes());
        b.push(prov);
        b.extend_from_slice(&val.to_bits().to_le_bytes());
        b
    }

    /// A real, immutable pf-archive-1 fixture (hand-encoded, not produced by
    /// the current v2 writer) must load through the v1 reader with the
    /// expected metric map, provenance, missing values, and dynamic metrics.
    /// This pins the v1 compatibility claim to a concrete artifact.
    #[test]
    fn real_v1_fixture_loads_with_expected_evidence() {
        let manifest = "{\"format\":\"pf-archive-1\",\"schema\":0,\"metrics\":[\
            {\"id\":0,\"collector\":\"battery\",\"metric\":\"discharge_w\",\"extra\":\"\"},\
            {\"id\":3,\"collector\":\"battery\",\"metric\":\"charge_pct\",\"extra\":\"\"},\
            {\"id\":4,\"collector\":\"cpu\",\"metric\":\"utility\",\"extra\":\"\"},\
            {\"id\":1000,\"collector\":\"gpu\",\"metric\":\"total_util_pct\",\"extra\":\"dGPU\"},\
            {\"id\":1001,\"collector\":\"proc\",\"metric\":\"cpu_pct\",\"extra\":\"42:game.exe\"}],\
            \"footer\":{\"discharge_wh\":0.05}}";
        let mut samples = Vec::new();
        samples.extend(v1_record(1000, 0, 0, 6.5)); // measured battery discharge
        samples.extend(v1_record(1000, 3, 9, 55.0)); // unknown prov -> Unavailable
        samples.extend(v1_record(1000, 4, 1, 12.0)); // derived cpu utility
        samples.extend(v1_record(1000, 1000, 1, 3.0)); // derived gpu dynamic
        samples.extend(v1_record(1000, 1001, 1, 1.5)); // derived proc dynamic
        samples.extend(v1_record(2000, 0, 0, 7.0)); // only battery at second tick
        let events = vec![
            "{\"wall_ms\":1500,\"mono_ms\":1000,\"kind\":\"marker\",\"detail\":\"v1\"}".to_string(),
        ];
        let s = load_archive("v1", manifest, &samples, &events).unwrap();
        assert_eq!(s.battery.len(), 2);
        assert_eq!(s.battery[0].discharge.val(), Some(6.5));
        assert_eq!(s.battery[0].discharge.provenance(), Provenance::Measured);
        assert_eq!(s.battery[0].pct.val(), None);
        assert_eq!(
            s.battery[0].pct.provenance(),
            Provenance::Unavailable,
            "unknown prov code must demote, never fabricate measured"
        );
        assert_eq!(s.cpu[0].utility.val(), Some(12.0));
        assert_eq!(s.cpu[0].utility.provenance(), Provenance::Derived);
        // Missing at the second tick: explicit unavailable, not zero.
        assert_eq!(s.cpu[1].utility.val(), None);
        assert_eq!(s.cpu[1].utility.provenance(), Provenance::Unavailable);
        assert_eq!(s.gpu.len(), 1);
        assert_eq!(s.gpu[0].adapters[0].name, "dGPU");
        assert_eq!(s.gpu[0].adapters[0].total_util.val(), Some(3.0));
        assert_eq!(
            s.gpu[0].adapters[0].total_util.provenance(),
            Provenance::Derived
        );
        assert_eq!(s.procs[0].top[0].pid, 42);
        assert_eq!(s.procs[0].top[0].name, "game.exe");
        assert_eq!(s.procs[0].top[0].cpu.provenance(), Provenance::Derived);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, "marker");
        assert!(s.footer.is_some());
        // A v2 manifest is not silently accepted by the v1 reader.
        assert!(
            load_archive(
                "v1",
                &manifest.replace("pf-archive-1", "pf-archive-2"),
                &samples,
                &events
            )
            .is_err()
        );
    }

    /// `source_hash` is an identity fingerprint, not integrity: identical
    /// identity yields the same value, different identity differs, and the
    /// loader never accepts or rejects on it.
    #[test]
    fn source_fingerprint_is_identity_only_not_integrity() {
        let s = demo_session();
        let a = migrate("demo", "demo.jsonl", &s);
        let a2 = migrate("demo", "demo.jsonl", &s);
        let h = |m: &str| {
            m.split("\"source_hash\":\"")
                .nth(1)
                .and_then(|r| r.split('"').next())
                .unwrap()
                .to_string()
        };
        assert_eq!(h(&a.manifest), h(&a2.manifest));
        assert!(!h(&a.manifest).is_empty());
        let other = migrate("different-label", "demo.jsonl", &s);
        assert_ne!(h(&a.manifest), h(&other.manifest));
        // Corrupting the fingerprint changes nothing about the loaded data.
        let wrong = a
            .manifest
            .replace("\"source_hash\":\"", "\"source_hash\":\"deadbeefdeadbeef");
        let good = load_archive_v2("demo", &a.manifest, &a.samples, &a.events).unwrap();
        let bad = load_archive_v2("demo", &wrong, &a.samples, &a.events).unwrap();
        assert_eq!(good.battery.len(), bad.battery.len());
        assert_eq!(
            good.battery[0].discharge.val(),
            bad.battery[0].discharge.val()
        );
        assert_eq!(
            good.battery[0].discharge.provenance(),
            bad.battery[0].discharge.provenance()
        );
    }
}
