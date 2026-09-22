//! Session loader: JSONL back into typed series for the analysis engine.
//!
//! The loader is tolerant (a corrupt line fails the load with its number,
//! it never silently invents points) and evidence-preserving: every value
//! becomes a Telemetry with provenance, kind, reason, source, timestamps
//! and quality intact.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::json::{JVal, parse};
use crate::telemetry::{ClockStamp, Reading, SampleQuality, Telemetry, UnavailKind};

/// Failure to parse a stored reason must not lose the absence itself:
/// known strings map to 'static constants, anything else is interned
/// (capped) so Unavailable(reason) stays factory-faithful.
pub(crate) fn intern_reason(s: &str) -> &'static str {
    match s {
        "no system battery" => "no system battery",
        "not reported by battery interface" => "not reported by battery interface",
        "not discharging (on AC or idle)" => "not discharging (on AC or idle)",
        "not charging" => "not charging",
        "design capacity unknown" => "design capacity unknown",
        "counter missing or not ready" => "counter missing or not ready",
        "energy instance missing or warming up" => "energy instance missing or warming up",
        "warming up (first tick)" => "warming up (first tick)",
        "process inaccessible (protected/system)" => "process inaccessible (protected/system)",
        "setting not present in active scheme" => "setting not present in active scheme",
        "no stored override" => "no stored override",
        "active scheme unknown" => "active scheme unknown",
        "PowerGetActiveScheme failed" => "PowerGetActiveScheme failed",
        "NtQueryTimerResolution failed" => "NtQueryTimerResolution failed",
        "no WMI sensor matched" => "no WMI sensor matched",
        "WMI brightness query failed" => "WMI brightness query failed",
        "counter not ready" => "counter not ready",
        "no device evidence in usb group" => "no device evidence in usb group",
        "no reason recorded" => "no reason recorded",
        "present value with unknown provenance kind" => {
            "present value with unknown provenance kind"
        }
        _ => {
            let mut owned = s.to_string();
            owned.truncate(256);
            Box::leak(owned.into_boxed_str())
        }
    }
}

/// Build full evidence for one envelope field. Present values with a known
/// kind keep it; present values with a missing/unknown kind are demoted to
/// Unavailable (never defaulted to Measured) with the value dropped and a
/// recorded reason — provenance must not be fabricated.
fn ev(obj: &JVal, key: &str, source: &'static str, stamp: ClockStamp) -> Telemetry<f64> {
    ev_inner(obj.get(key), source, stamp)
}

/// Build evidence from an envelope object directly (for nested envelopes
/// like the {"ac","dc"} policy pairs). Shared with the archive loader.
pub(crate) fn ev_inner(
    inner: Option<&JVal>,
    source: &'static str,
    stamp: ClockStamp,
) -> Telemetry<f64> {
    // Backward compatibility: some writers emit plain numbers (notably
    // older net totals). A bare number was always a measured value, but
    // its freshness is unknown — keep the value, mark quality Unknown.
    if let Some(n) = inner.and_then(|v| v.num()) {
        return Telemetry {
            value: Reading::Measured(n),
            source,
            stamp,
            quality: SampleQuality::Unknown,
            unavail_kind: None,
        };
    }
    let v = inner.and_then(|o| o.get("v")).and_then(|n| n.num());
    let p = inner.and_then(|o| o.get("p")).and_then(|s| s.as_str());
    let reason = inner
        .and_then(|o| o.get("reason"))
        .and_then(|r| r.as_str())
        .unwrap_or("no reason recorded");
    let kind = inner
        .and_then(|o| o.get("k"))
        .and_then(|k| k.num())
        .map(|k| UnavailKind::from_u8(k as u8))
        .unwrap_or(UnavailKind::NotSampled);
    let quality = inner
        .and_then(|o| o.get("q"))
        .and_then(|q| q.num())
        .map(|q| SampleQuality::from_u8(q as u8))
        .unwrap_or(SampleQuality::Unknown);
    let fresh_unless_known = |q: SampleQuality| {
        if q == SampleQuality::Unknown {
            SampleQuality::Fresh
        } else {
            q
        }
    };
    match (v, p) {
        (Some(x), Some("measured")) => Telemetry {
            value: Reading::Measured(x),
            source,
            stamp,
            quality: fresh_unless_known(quality),
            unavail_kind: None,
        },
        (Some(x), Some("derived")) => Telemetry {
            value: Reading::Derived(x),
            source,
            stamp,
            quality: fresh_unless_known(quality),
            unavail_kind: None,
        },
        (Some(x), Some("estimated")) => Telemetry {
            value: Reading::Estimated(x),
            source,
            stamp,
            quality: fresh_unless_known(quality),
            unavail_kind: None,
        },
        (Some(_), _) => Telemetry {
            value: Reading::Unavailable(intern_reason(
                "present value with unknown provenance kind",
            )),
            source,
            stamp,
            quality: SampleQuality::Unknown,
            unavail_kind: Some(UnavailKind::NotSampled),
        },
        (None, _) => Telemetry {
            value: Reading::Unavailable(intern_reason(reason)),
            source,
            stamp,
            quality,
            unavail_kind: Some(kind),
        },
    }
}

/// Boolean envelope loader (e.g. battery `ac`, whose "v" is a JSON bool).
/// A present bool is a measured 1.0/0.0; a null envelope keeps its stored
/// absence reason. Never fabricates Measured from a missing key.
pub(crate) fn ev_bool(
    obj: &JVal,
    key: &str,
    source: &'static str,
    stamp: ClockStamp,
) -> Telemetry<f64> {
    match obj.get(key) {
        Some(o) => {
            let b = o.as_bool().or_else(|| o.get("v").and_then(|x| x.as_bool()));
            match b {
                Some(b) => Telemetry::measured(if b { 1.0 } else { 0.0 }, source, stamp),
                None => ev_inner(Some(o), source, stamp),
            }
        }
        None => Telemetry::unavailable(UnavailKind::NotSampled, "field missing", source, stamp),
    }
}

#[derive(Debug, Clone, Default)]
pub struct BatteryPoint {
    pub t: f64,
    pub discharge: Telemetry<f64>,
    pub charge: Telemetry<f64>,
    pub remaining_wh: Telemetry<f64>,
    pub pct: Telemetry<f64>,
}

/// Per-tick battery evidence that must not disturb `BatteryPoint` (an
/// external crate constructs it exhaustively): AC state, signed rate,
/// health, raw temperature, and the acquisition window.
#[derive(Debug, Clone, Default)]
pub struct BatteryExtraPoint {
    pub t: f64,
    pub ac: Telemetry<f64>,
    pub rate_mw: Telemetry<f64>,
    pub health: Telemetry<f64>,
    pub temperature_raw: Telemetry<f64>,
    /// Monotonic acquisition-window bounds as reported by the collector.
    /// `None` when the collector did not emit them — never 0: an absent
    /// window must stay distinguishable from a real t=0 boundary.
    pub t_start_ms: Option<u64>,
    pub t_end_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct CpuCorePoint {
    pub instance: String,
    pub utility: Telemetry<f64>,
    pub performance: Telemetry<f64>,
    pub freq_mhz: Telemetry<f64>,
    pub parking: Telemetry<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct CpuPoint {
    pub t: f64,
    /// Monotonic acquisition-window bounds; `None` when not reported.
    pub t_start_ms: Option<u64>,
    pub t_end_ms: Option<u64>,
    pub utility: Telemetry<f64>,
    pub c3_pct: Telemetry<f64>,
    pub idle_breaks: Telemetry<f64>,
    pub pkg_derived_w: Telemetry<f64>,
    /// Measured package power (Energy Meter Power counter), distinct from
    /// the derived cross-check.
    pub pkg_power_w: Telemetry<f64>,
    /// System context-switch rate (wakeup-storm signal).
    pub ctx_switches: Telemetry<f64>,
    /// Per-core instances (enumerated at runtime; never hard-coded).
    pub cores: Vec<CpuCorePoint>,
    /// Every remaining advertised total counter, by wire key, with full
    /// provenance (utility/c3_pct/idle_breaks/ctx_switches live above).
    pub totals: Vec<(String, Telemetry<f64>)>,
}

#[derive(Debug, Clone, Default)]
pub struct AwakeRec {
    pub active: bool,
    pub confidence: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Default)]
pub struct GpuAdapterPoint {
    pub name: String,
    pub discrete: bool,
    pub total_util: Telemetry<f64>,
    pub mem_dedicated_mb: Telemetry<f64>,
    pub mem_shared_mb: Telemetry<f64>,
    pub util_3d: Telemetry<f64>,
    pub util_compute: Telemetry<f64>,
    pub util_decode: Telemetry<f64>,
    pub util_codec: Telemetry<f64>,
    pub util_copy: Telemetry<f64>,
    pub util_other: Telemetry<f64>,
    pub engines_active: Telemetry<f64>,
    pub engines_seen: Telemetry<f64>,
    /// Awake verdict (activity proves awake; silence proves nothing).
    pub awake: Option<AwakeRec>,
    /// Top PIDs by engine-%, with telemetry per PID.
    pub top_pids: Vec<(u32, Telemetry<f64>)>,
}

#[derive(Debug, Clone, Default)]
pub struct GpuPoint {
    pub t: f64,
    /// Monotonic acquisition-window bounds; `None` when not reported.
    pub t_start_ms: Option<u64>,
    pub t_end_ms: Option<u64>,
    /// True when the last counter rebuild failed and a stale set is in use.
    /// Staleness MUST survive to SessionData and the archive.
    pub stale: bool,
    pub adapters: Vec<GpuAdapterPoint>,
}

#[derive(Debug, Clone, Default)]
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub start_unix_ms: Option<u64>,
    /// Tick the process exited (unix ms), when reported. None means still
    /// running or unknown — absence from top-N is NOT proof of exit.
    pub exit_unix_ms: Option<u64>,
    pub cpu: Telemetry<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct ProcPoint {
    pub t: f64,
    /// Monotonic acquisition-window bounds; `None` when not reported.
    pub t_start_ms: Option<u64>,
    pub t_end_ms: Option<u64>,
    /// System-wide thread count when reported. `None` means unknown, never a
    /// fabricated 0 (legacy records carry no coverage metadata).
    pub total_threads: Option<u64>,
    /// Processes the snapshot could not inspect (protected/system). This is
    /// evidence about enumeration completeness: `None` = unknown, `Some(0)`
    /// = every process inspected, `Some(n)` = n processes unreadable. It must
    /// never be collapsed to 0, which would claim complete observation.
    pub inaccessible: Option<usize>,
    /// True when the retained top-N process list was capped below the full
    /// process set. `None` = unknown, never "complete".
    pub truncated: Option<bool>,
    pub top: Vec<ProcEntry>,
}

impl SessionData {
    /// Tri-state process-observation completeness across the session.
    ///
    /// `Some(true)`  = coverage metadata exists and at least one tick had
    ///                 inaccessible or truncated process observation;
    /// `Some(false)` = coverage metadata exists and every observed tick was
    ///                 complete;
    /// `None`        = no coverage metadata at all (older files), which is
    ///                 UNKNOWN completeness and must not be read as complete.
    pub fn process_coverage_incomplete(&self) -> Option<bool> {
        let mut any_known = false;
        for p in &self.procs {
            if p.inaccessible.is_none() && p.truncated.is_none() {
                continue;
            }
            any_known = true;
            if p.inaccessible.unwrap_or(0) > 0 || p.truncated.unwrap_or(false) {
                return Some(true);
            }
        }
        any_known.then_some(false)
    }
}

#[derive(Debug, Clone, Default)]
pub struct DisplayEntry {
    pub name: String,
    pub primary: bool,
    /// Mode dimensions. `None` means the collector did not report them;
    /// it is never a fabricated zero. A reported zero stays `Some(0)`.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub freq_hz: Option<u32>,
    pub bpp: Option<u32>,
    pub brightness: Telemetry<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct HdrTargetRec {
    /// "adapter_low/adapter_high" identity from DisplayConfig.
    pub adapter: String,
    pub target: u32,
    pub supported: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DisplayPoint {
    pub t: f64,
    /// Monotonic acquisition-window bounds; `None` when not reported.
    pub t_start_ms: Option<u64>,
    pub t_end_ms: Option<u64>,
    /// First display's brightness (backward-compatible view).
    pub brightness: Telemetry<f64>,
    /// Every attached display, with stable identity.
    pub displays: Vec<DisplayEntry>,
    /// Brightness sensors with no matching display: (instance, brightness).
    /// Brightness is `None` when not reported, never a fabricated zero.
    pub unmatched_sensors: Vec<(String, Option<f64>)>,
    pub hdr_targets: Vec<HdrTargetRec>,
    /// Presentation-only: WMI/DisplayConfig error text.
    pub hdr_error: Option<String>,
    /// Presentation-only: human change descriptions (also emitted as events).
    pub changes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct NetAdapterRec {
    pub alias: String,
    pub descr: String,
    pub class: String,
    pub iftype: String,
    /// Oper/media state codes. `None` when the collector did not report them;
    /// never a fabricated zero.
    pub oper: Option<f64>,
    pub media: Option<f64>,
    pub admin_up: bool,
    pub tx_mbps: Telemetry<f64>,
    pub rx_mbps: Telemetry<f64>,
    pub in_octets: Telemetry<f64>,
    pub out_octets: Telemetry<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct WifiRec {
    pub descr: String,
    /// Wi-Fi state code. `None` when not reported; never a fabricated zero.
    pub state: Option<f64>,
    pub ssid: String,
    pub signal_pct: Telemetry<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct NetPoint {
    pub t: f64,
    /// Monotonic acquisition-window bounds; `None` when not reported.
    pub t_start_ms: Option<u64>,
    pub t_end_ms: Option<u64>,
    pub total_rx: Telemetry<f64>,
    pub total_tx: Telemetry<f64>,
    pub adapters: Vec<NetAdapterRec>,
    /// Per-interface throughput, keyed by PDH instance name.
    pub throughput: Vec<(String, Telemetry<f64>, Telemetry<f64>)>,
    pub wifi: Vec<WifiRec>,
    /// Presentation-only: oper/media transition descriptions.
    pub changes: Vec<String>,
    /// True when the counter-set rebuild failed and a previous interface
    /// topology is still in use (topology not current, even if values are).
    pub topology_stale: bool,
}

#[derive(Debug, Clone, Default)]
pub struct StoragePoint {
    pub t: f64,
    /// Monotonic acquisition-window bounds; `None` when not reported.
    pub t_start_ms: Option<u64>,
    pub t_end_ms: Option<u64>,
    pub disk_time: Telemetry<f64>,
    pub read_bps: Telemetry<f64>,
    pub write_bps: Telemetry<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct UsbClassCount {
    pub class: String,
    pub count: usize,
    pub evidence: Telemetry<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct UsbPoint {
    pub t: f64,
    /// Monotonic acquisition-window bounds; `None` when not reported.
    pub t_start_ms: Option<u64>,
    pub t_end_ms: Option<u64>,
    pub classes: Vec<UsbClassCount>,
    /// Presentation-only: devices with declared power requirements.
    pub power_devices: Vec<String>,
    /// Presentation-only: DevicePowerEnumDevices error text.
    pub power_error: Option<String>,
    /// Presentation-only: connect/disconnect descriptions.
    pub changes: Vec<String>,
}

impl UsbPoint {
    /// Backward-compat view: (class, count) pairs without evidence.
    pub fn counts(&self) -> Vec<(String, usize)> {
        self.classes
            .iter()
            .map(|c| (c.class.clone(), c.count))
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct SelfPoint {
    pub t: f64,
    /// Monotonic acquisition-window bounds; `None` when not reported.
    pub t_start_ms: Option<u64>,
    pub t_end_ms: Option<u64>,
    pub cpu_pct: Telemetry<f64>,
    pub ws_mb: Telemetry<f64>,
    pub priv_mb: Telemetry<f64>,
    pub io_read_b: Telemetry<f64>,
    pub io_write_b: Telemetry<f64>,
    pub threads: Telemetry<f64>,
    pub ctx_switches: Telemetry<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub scheme_name: Option<String>,
    pub scheme_guid: Option<String>,
    /// Wall ms of the powercfg effective-value snapshot (0 = none taken).
    pub policy_snapshot_wall_ms: Option<u64>,
    pub timer_resolution: Telemetry<f64>,
    pub cpu_min_ac: Telemetry<f64>,
    pub cpu_max_ac: Telemetry<f64>,
    pub cpu_min_dc: Telemetry<f64>,
    pub cpu_max_dc: Telemetry<f64>,
    pub epp_ac: Telemetry<f64>,
    pub epp_dc: Telemetry<f64>,
    pub brightness_ac: Telemetry<f64>,
    pub brightness_dc: Telemetry<f64>,
    pub display_timeout_ac: Telemetry<f64>,
    pub display_timeout_dc: Telemetry<f64>,
    pub sleep_timeout_ac: Telemetry<f64>,
    pub sleep_timeout_dc: Telemetry<f64>,
    pub hibernate_timeout_ac: Telemetry<f64>,
    pub hibernate_timeout_dc: Telemetry<f64>,
    pub low_batt_ac: Telemetry<f64>,
    pub low_batt_dc: Telemetry<f64>,
    pub crit_batt_ac: Telemetry<f64>,
    pub crit_batt_dc: Telemetry<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatteryStaticEntry {
    pub index: u32,
    pub designed_mwh: Option<f64>,
    pub full_mwh: Option<f64>,
    pub cycle_count: Option<u32>,
    pub chemistry: Option<String>,
    pub device_name: Option<String>,
    pub unique_id: Option<String>,
    pub technology: Option<u32>,
    pub capabilities: Option<u32>,
    /// Raw temperature ULONG (units unverified by the collector).
    pub temperature_raw: Option<u32>,
    pub mfg_name: Option<String>,
    /// "YYYY-MM-DD" when the interface reports one.
    pub mfg_date: Option<String>,
    /// Provenance of designed_mwh / full_mwh ("measured"|"derived"|
    /// "estimated"|"unavailable"). Unknown or missing -> "unavailable":
    /// provenance is never fabricated.
    pub design_prov: String,
    pub full_prov: String,
}

impl Default for BatteryStaticEntry {
    fn default() -> Self {
        BatteryStaticEntry {
            index: 0,
            designed_mwh: None,
            full_mwh: None,
            cycle_count: None,
            chemistry: None,
            device_name: None,
            unique_id: None,
            technology: None,
            capabilities: None,
            temperature_raw: None,
            mfg_name: None,
            mfg_date: None,
            design_prov: "unavailable".to_string(),
            full_prov: "unavailable".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BatteryMeta {
    pub full_mwh: Option<f64>,
    pub design_mwh: Option<f64>,
    pub battery_index: Option<u32>,
    pub battery_count: Option<u32>,
    /// Provenance of the headline full/design values (same vocabulary as
    /// above; unknown or missing -> "unavailable").
    pub full_prov: String,
    pub design_prov: String,
    /// One entry per physical battery interface actually queried.
    pub batteries: Vec<BatteryStaticEntry>,
}

impl Default for BatteryMeta {
    fn default() -> Self {
        BatteryMeta {
            full_mwh: None,
            design_mwh: None,
            battery_index: None,
            battery_count: None,
            full_prov: "unavailable".to_string(),
            design_prov: "unavailable".to_string(),
            batteries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EventRec {
    pub wall_ms: u64,
    pub mono_ms: u64,
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct SessionData {
    pub label: String,
    /// Wall ms of the first line seen; archive wall times derive from it.
    pub wall_base_ms: u64,
    pub battery: Vec<BatteryPoint>,
    /// Per-tick battery evidence (AC/rate/health/temperature/timing), kept
    /// alongside `BatteryPoint` for layout stability of that public type.
    pub battery_extra: Vec<BatteryExtraPoint>,
    pub cpu: Vec<CpuPoint>,
    pub gpu: Vec<GpuPoint>,
    pub procs: Vec<ProcPoint>,
    pub display: Vec<DisplayPoint>,
    pub net: Vec<NetPoint>,
    pub storage: Vec<StoragePoint>,
    pub usb: Vec<UsbPoint>,
    pub selfmon: Vec<SelfPoint>,
    pub policy: Policy,
    /// Every os_power snapshot in order (headline `policy` stays the first
    /// for compatibility; history enables change detection).
    pub policy_history: Vec<(f64, Policy)>,
    pub battery_meta: BatteryMeta,
    pub events: Vec<EventRec>,
    /// Raw footer summary object (flat numbers-or-nulls), if present.
    pub footer: Option<JVal>,
}

fn mono_s(v: &JVal) -> f64 {
    v.get("mono_ms").and_then(|m| m.num()).unwrap_or(0.0) / 1000.0
}

fn wall_ms(v: &JVal) -> u64 {
    v.get("wall_ms").and_then(|m| m.num()).unwrap_or(0.0) as u64
}

/// Optional monotonic acquisition-window bound. Missing stays `None`; a
/// collector that did not report its window must not look like a real
/// t_start/t_end of 0.
fn opt_ms(v: &JVal, key: &str) -> Option<u64> {
    v.get(key).and_then(|n| n.num()).map(|n| n as u64)
}

fn as_u32(v: &JVal) -> Option<u32> {
    v.num().map(|n| n as u32)
}

fn stamp_of(v: &JVal) -> ClockStamp {
    ClockStamp {
        wall_millis: wall_ms(v),
        mono_millis: (mono_s(v) * 1000.0) as u64,
    }
}

/// Provenance string of one envelope field ("measured"|"derived"|
/// "estimated"|"unavailable"). Missing or unknown -> "unavailable":
/// provenance is never fabricated.
fn prov_of(v: &JVal, key: &str) -> String {
    match v.get(key).and_then(|f| f.get("p")).and_then(|p| p.as_str()) {
        Some("measured") | Some("derived") | Some("estimated") | Some("unavailable") => v
            .get(key)
            .and_then(|f| f.get("p"))
            .and_then(|p| p.as_str())
            .unwrap_or("unavailable")
            .to_string(),
        _ => "unavailable".to_string(),
    }
}

fn load_battery(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    let t = stamp.mono_millis as f64 / 1000.0;
    if s.battery_meta.full_mwh.is_none() {
        s.battery_meta.full_mwh = v.get("full_charge_mwh").and_then(|f| f.get("v")?.num());
        s.battery_meta.full_prov = prov_of(v, "full_charge_mwh");
    }
    if s.battery_meta.design_mwh.is_none() {
        s.battery_meta.design_mwh = v.get("design_mwh").and_then(|f| f.get("v")?.num());
        s.battery_meta.design_prov = prov_of(v, "design_mwh");
    }
    if s.battery_meta.battery_count.is_none() {
        s.battery_meta.battery_count = v
            .get("battery_count")
            .and_then(|f| f.get("v")?.num())
            .map(|n| n as u32);
        s.battery_meta.battery_index = v
            .get("battery_index")
            .and_then(|f| f.get("v")?.num())
            .map(|n| n as u32);
    }
    if s.battery_meta.batteries.is_empty()
        && let Some(arr) = v.get("batteries").and_then(|b| b.arr())
    {
        let num = |o: &JVal, k: &str| o.get(k).and_then(|f| f.get("v")?.num());
        let text = |o: &JVal, k: &str| {
            o.get(k)
                .and_then(|f| f.get("v")?.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        };
        s.battery_meta.batteries = arr
            .iter()
            .map(|b| BatteryStaticEntry {
                index: b.get("index").and_then(|i| i.num()).unwrap_or(0.0) as u32,
                designed_mwh: num(b, "designed_mwh"),
                full_mwh: num(b, "full_mwh"),
                cycle_count: num(b, "cycle_count").map(|c| c as u32),
                chemistry: text(b, "chemistry"),
                device_name: text(b, "device_name"),
                unique_id: text(b, "unique_id"),
                technology: num(b, "technology").map(|x| x as u32),
                capabilities: num(b, "capabilities").map(|x| x as u32),
                temperature_raw: num(b, "temperature_raw").map(|x| x as u32),
                mfg_name: text(b, "mfg_name"),
                mfg_date: text(b, "mfg_date"),
                design_prov: prov_of(b, "designed_mwh"),
                full_prov: prov_of(b, "full_mwh"),
            })
            .collect();
    }
    // The collector reports technology/capabilities/temperature/identity at
    // the top level for the primary battery only; attach them to the first
    // queried interface when they are present there.
    if let Some(first) = s.battery_meta.batteries.first_mut() {
        let tstr = |k: &str| {
            v.get(k)
                .and_then(|f| f.get("v")?.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        };
        let tnum = |k: &str| v.get(k).and_then(|f| f.get("v")?.num()).map(|x| x as u32);
        if first.technology.is_none() {
            first.technology = tnum("technology");
        }
        if first.capabilities.is_none() {
            first.capabilities = tnum("capabilities");
        }
        if first.temperature_raw.is_none() {
            first.temperature_raw = tnum("temperature_raw");
        }
        if first.mfg_name.is_none() {
            first.mfg_name = tstr("mfg_name");
        }
        if first.mfg_date.is_none() {
            first.mfg_date = tstr("mfg_date");
        }
    }
    // remaining_mwh is milliwatt-hours on the wire; the loader scales to
    // watt-hours, which is a derivation — the scaled copy is Derived even
    // when the wire value was measured.
    let rem_mwh = ev(v, "remaining_mwh", "battery", stamp);
    let remaining_wh = Telemetry {
        value: match rem_mwh.value {
            Reading::Measured(x) => Reading::Derived(x / 1000.0),
            Reading::Derived(x) => Reading::Derived(x / 1000.0),
            Reading::Estimated(x) => Reading::Estimated(x / 1000.0),
            Reading::Unavailable(r) => Reading::Unavailable(r),
        },
        ..rem_mwh
    };
    s.battery.push(BatteryPoint {
        t,
        discharge: ev(v, "discharge_w", "battery", stamp),
        charge: ev(v, "charge_w", "battery", stamp),
        remaining_wh,
        pct: ev(v, "charge_pct", "battery", stamp),
    });
    s.battery_extra.push(BatteryExtraPoint {
        t,
        ac: ev_bool(v, "ac", "battery", stamp),
        rate_mw: ev(v, "rate_mw", "battery", stamp),
        health: ev(v, "health", "battery", stamp),
        temperature_raw: ev(v, "temperature_raw", "battery", stamp),
        t_start_ms: opt_ms(v, "t_start_ms"),
        t_end_ms: opt_ms(v, "t_end_ms"),
    });
}

fn load_cpu(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    let t = stamp.mono_millis as f64 / 1000.0;
    let totals = v.get("totals");
    let energy = v.get("energy");
    let sub = |parent: Option<&JVal>, key: &str| -> Telemetry<f64> {
        match parent {
            Some(o) => ev(o, key, "cpu", stamp),
            None => Telemetry::unavailable(
                UnavailKind::NotSampled,
                "parent object missing",
                "cpu",
                stamp,
            ),
        }
    };
    // Every advertised total counter except the dedicated fields above is
    // retained by wire key with full provenance, so no advertised counter is
    // silently dropped.
    let mut extra: Vec<(String, Telemetry<f64>)> = Vec::new();
    if let (Some(JVal::Obj(pairs)), Some(tot)) = (totals, totals) {
        for (k, _) in pairs {
            if matches!(
                k.as_str(),
                "utility" | "c3_pct" | "idle_breaks" | "ctx_switches"
            ) {
                continue;
            }
            extra.push((k.clone(), ev(tot, k, "cpu", stamp)));
        }
    }
    let mut cores = Vec::new();
    if let Some(list) = v.get("cores").and_then(|c| c.arr()) {
        for c in list {
            cores.push(CpuCorePoint {
                instance: c
                    .get("inst")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?")
                    .to_string(),
                utility: ev(c, "utility", "cpu", stamp),
                performance: ev(c, "performance", "cpu", stamp),
                freq_mhz: ev(c, "freq_mhz", "cpu", stamp),
                parking: ev(c, "parking", "cpu", stamp),
            });
        }
    }
    s.cpu.push(CpuPoint {
        t,
        t_start_ms: opt_ms(v, "t_start_ms"),
        t_end_ms: opt_ms(v, "t_end_ms"),
        utility: sub(totals, "utility"),
        c3_pct: sub(totals, "c3_pct"),
        idle_breaks: sub(totals, "idle_breaks"),
        pkg_derived_w: sub(energy, "pkg_derived_w"),
        pkg_power_w: sub(energy, "pkg_power_w"),
        ctx_switches: sub(totals, "ctx_switches"),
        cores,
        totals: extra,
    });
}

fn load_gpu(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    let t = stamp.mono_millis as f64 / 1000.0;
    let mut adapters = Vec::new();
    if let Some(list) = v.get("adapters").and_then(|a| a.arr()) {
        for a in list {
            let mut top_pids = Vec::new();
            if let Some(ps) = a.get("top_pids").and_then(|x| x.arr()) {
                for p in ps {
                    top_pids.push((
                        p.get("pid").and_then(as_u32).unwrap_or(0),
                        ev(p, "util_pct", "gpu", stamp),
                    ));
                }
            }
            let awake = a.get("awake").map(|aw| AwakeRec {
                active: aw.get("active").and_then(|x| x.as_bool()).unwrap_or(false),
                confidence: aw
                    .get("confidence")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                evidence: aw
                    .get("evidence")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
            adapters.push(GpuAdapterPoint {
                name: a
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string(),
                discrete: a.get("discrete").and_then(|d| d.as_bool()).unwrap_or(false),
                total_util: ev(a, "total_util_pct", "gpu", stamp),
                mem_dedicated_mb: ev(a, "mem_dedicated_mb", "gpu", stamp),
                mem_shared_mb: ev(a, "mem_shared_mb", "gpu", stamp),
                util_3d: ev(a, "util_3d", "gpu", stamp),
                util_compute: ev(a, "util_compute", "gpu", stamp),
                util_decode: ev(a, "util_decode", "gpu", stamp),
                util_codec: ev(a, "util_codec", "gpu", stamp),
                util_copy: ev(a, "util_copy", "gpu", stamp),
                util_other: ev(a, "util_other", "gpu", stamp),
                engines_active: ev(a, "engines_active", "gpu", stamp),
                engines_seen: ev(a, "engines_seen", "gpu", stamp),
                awake,
                top_pids,
            });
        }
    }
    s.gpu.push(GpuPoint {
        t,
        t_start_ms: opt_ms(v, "t_start_ms"),
        t_end_ms: opt_ms(v, "t_end_ms"),
        stale: v.get("stale").and_then(|x| x.as_bool()).unwrap_or(false),
        adapters,
    });
}

fn load_procs(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    let t = stamp.mono_millis as f64 / 1000.0;
    let mut top = Vec::new();
    if let Some(list) = v.get("top").and_then(|a| a.arr()) {
        for p in list {
            top.push(ProcEntry {
                pid: p.get("pid").and_then(as_u32).unwrap_or(0),
                ppid: p.get("ppid").and_then(as_u32).unwrap_or(0),
                name: p
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string(),
                start_unix_ms: p
                    .get("start_unix_ms")
                    .and_then(|n| n.num())
                    .map(|n| n as u64),
                exit_unix_ms: p
                    .get("exit_unix_ms")
                    .and_then(|n| n.num())
                    .map(|n| n as u64),
                cpu: ev(p, "cpu_pct", "proc", stamp),
            });
        }
    }
    s.procs.push(ProcPoint {
        t,
        t_start_ms: opt_ms(v, "t_start_ms"),
        t_end_ms: opt_ms(v, "t_end_ms"),
        // Coverage metadata: absent stays None (unknown), never a fabricated
        // 0/false that would claim complete process observation.
        total_threads: v
            .get("total_threads")
            .and_then(|n| n.num())
            .map(|n| n as u64),
        inaccessible: v
            .get("inaccessible")
            .and_then(|n| n.num())
            .map(|n| n as usize),
        truncated: v.get("truncated").and_then(|x| x.as_bool()),
        top,
    });
}

fn load_display(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    let t = stamp.mono_millis as f64 / 1000.0;
    let mut displays = Vec::new();
    if let Some(list) = v.get("displays").and_then(|d| d.arr()) {
        for d in list {
            displays.push(DisplayEntry {
                name: d
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .to_string(),
                primary: d.get("primary").and_then(|x| x.as_bool()).unwrap_or(false),
                width: d.get("width").and_then(|x| x.num()).map(|v| v as u32),
                height: d.get("height").and_then(|x| x.num()).map(|v| v as u32),
                freq_hz: d.get("freq_hz").and_then(|x| x.num()).map(|v| v as u32),
                bpp: d.get("bpp").and_then(|x| x.num()).map(|v| v as u32),
                brightness: ev(d, "brightness_pct", "display", stamp),
            });
        }
    }
    let brightness = displays
        .first()
        .map(|d| d.brightness.clone())
        .unwrap_or_else(|| {
            Telemetry::unavailable(
                UnavailKind::NotSampled,
                "no displays listed",
                "display",
                stamp,
            )
        });
    let mut unmatched_sensors = Vec::new();
    if let Some(list) = v.get("unmatched_sensors").and_then(|d| d.arr()) {
        for u in list {
            unmatched_sensors.push((
                u.get("instance")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string(),
                u.get("brightness").and_then(|x| x.num()),
            ));
        }
    }
    let mut hdr_targets = Vec::new();
    if let Some(list) = v.get("hdr_targets").and_then(|d| d.arr()) {
        for h in list {
            hdr_targets.push(HdrTargetRec {
                adapter: h
                    .get("adapter")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                target: h.get("target").and_then(|x| x.num()).unwrap_or(0.0) as u32,
                supported: h
                    .get("supported")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
                enabled: h.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
            });
        }
    }
    s.display.push(DisplayPoint {
        t,
        t_start_ms: opt_ms(v, "t_start_ms"),
        t_end_ms: opt_ms(v, "t_end_ms"),
        brightness,
        displays,
        unmatched_sensors,
        hdr_targets,
        hdr_error: v
            .get("hdr_error")
            .and_then(|x| x.as_str())
            .map(|x| x.to_string()),
        changes: str_list(v, "changes"),
    });
}

/// Extract an array of strings; empty when absent or malformed.
fn str_list(v: &JVal, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.arr())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn load_net(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    let t = stamp.mono_millis as f64 / 1000.0;
    let mut adapters = Vec::new();
    if let Some(list) = v.get("adapters").and_then(|a| a.arr()) {
        for a in list {
            adapters.push(NetAdapterRec {
                alias: a
                    .get("alias")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                descr: a
                    .get("descr")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                class: a
                    .get("class")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                iftype: a
                    .get("iftype")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                oper: a.get("oper").and_then(|x| x.num()),
                media: a.get("media").and_then(|x| x.as_str()).and_then(media_code),
                admin_up: a.get("admin_up").and_then(|x| x.as_bool()).unwrap_or(false),
                tx_mbps: plain_num(a, "tx_mbps", "net", stamp),
                rx_mbps: plain_num(a, "rx_mbps", "net", stamp),
                in_octets: plain_num(a, "in_octets", "net", stamp),
                out_octets: plain_num(a, "out_octets", "net", stamp),
            });
        }
    }
    let mut throughput = Vec::new();
    if let Some(list) = v.get("throughput").and_then(|a| a.arr()) {
        for p in list {
            throughput.push((
                p.get("instance")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                ev(p, "rx_bps", "net", stamp),
                ev(p, "tx_bps", "net", stamp),
            ));
        }
    }
    let mut wifi = Vec::new();
    if let Some(list) = v.get("wifi").and_then(|a| a.arr()) {
        for w in list {
            wifi.push(WifiRec {
                descr: w
                    .get("descr")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                state: w
                    .get("state")
                    .and_then(|x| x.as_str())
                    .and_then(wifi_state_code),
                ssid: w
                    .get("ssid")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                signal_pct: ev(w, "signal_pct", "net", stamp),
            });
        }
    }
    s.net.push(NetPoint {
        t,
        t_start_ms: opt_ms(v, "t_start_ms"),
        t_end_ms: opt_ms(v, "t_end_ms"),
        total_rx: ev(v, "total_rx_bps", "net", stamp),
        total_tx: ev(v, "total_tx_bps", "net", stamp),
        adapters,
        throughput,
        wifi,
        changes: str_list(v, "changes"),
        topology_stale: v
            .get("topology_stale")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
    });
}

/// Plain (non-envelope) numeric fields: a present number is Measured with
/// Unknown freshness (its writer carries no quality); absent is Unavailable.
fn plain_num(obj: &JVal, key: &str, source: &'static str, stamp: ClockStamp) -> Telemetry<f64> {
    match obj.get(key) {
        Some(v) => {
            if let Some(n) = v.num() {
                return Telemetry {
                    value: Reading::Measured(n),
                    source,
                    stamp,
                    quality: SampleQuality::Unknown,
                    unavail_kind: None,
                };
            }
            ev_inner(Some(v), source, stamp)
        }
        None => Telemetry::unavailable(UnavailKind::NotSampled, "field missing", source, stamp),
    }
}

fn media_code(s: &str) -> Option<f64> {
    match s {
        "Unknown" => Some(0.0),
        "Connected" => Some(1.0),
        "Disconnected" => Some(2.0),
        _ => None,
    }
}

fn wifi_state_code(s: &str) -> Option<f64> {
    match s {
        "not-ready" => Some(0.0),
        "connected" => Some(1.0),
        "adhoc" => Some(2.0),
        "disconnecting" => Some(3.0),
        "disconnected" => Some(4.0),
        "associating" => Some(5.0),
        "discovering" => Some(6.0),
        "authenticating" => Some(7.0),
        _ => None,
    }
}

fn load_storage(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    let t = stamp.mono_millis as f64 / 1000.0;
    let total = v
        .get("disks")
        .and_then(|d| d.arr())
        .and_then(|list| {
            list.iter().find(|d| {
                d.get("total").and_then(|x| x.as_bool()).unwrap_or(false)
                    || d.get("instance").and_then(|x| x.as_str()) == Some("_Total")
            })
        })
        .cloned()
        .unwrap_or(JVal::Null);
    s.storage.push(StoragePoint {
        t,
        t_start_ms: opt_ms(v, "t_start_ms"),
        t_end_ms: opt_ms(v, "t_end_ms"),
        disk_time: ev(&total, "disk_time_pct", "storage", stamp),
        read_bps: ev(&total, "read_bps", "storage", stamp),
        write_bps: ev(&total, "write_bps", "storage", stamp),
    });
}

fn load_usb(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    let t = stamp.mono_millis as f64 / 1000.0;
    let mut classes = Vec::new();
    if let Some(groups) = v.get("groups").and_then(|g| g.arr()) {
        for g in groups {
            let class = g
                .get("class")
                .and_then(|c| c.as_str())
                .unwrap_or("?")
                .to_string();
            let devices = g.get("devices").and_then(|d| d.arr());
            let n = devices.map(|a| a.len()).unwrap_or(0);
            // Per-group count envelope carries the evidence when present.
            // No envelope: the count we can form comes from this writer's
            // own devices[] evidence, so it is DERIVED (a counted length is
            // not a raw measurement). If the group carries no devices[]
            // evidence at all, the count is Unavailable — never Measured
            // and never a silently fabricated 0.
            let evidence = match (g.get("count"), devices) {
                (Some(_), _) => ev(g, "count", "usb", stamp),
                (None, Some(_)) => Telemetry::derived(n as f64, "usb", stamp),
                (None, None) => Telemetry::unavailable(
                    UnavailKind::NotSampled,
                    "no device evidence in usb group",
                    "usb",
                    stamp,
                ),
            };
            classes.push(UsbClassCount {
                class,
                count: n,
                evidence,
            });
        }
    }
    s.usb.push(UsbPoint {
        t,
        t_start_ms: opt_ms(v, "t_start_ms"),
        t_end_ms: opt_ms(v, "t_end_ms"),
        classes,
        power_devices: str_list(v, "power_devices"),
        power_error: v
            .get("power_error")
            .and_then(|x| x.as_str())
            .map(|x| x.to_string()),
        changes: str_list(v, "changes"),
    });
}

fn load_selfmon(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    let t = stamp.mono_millis as f64 / 1000.0;
    s.selfmon.push(SelfPoint {
        t,
        t_start_ms: opt_ms(v, "t_start_ms"),
        t_end_ms: opt_ms(v, "t_end_ms"),
        cpu_pct: ev(v, "cpu_pct", "self", stamp),
        ws_mb: ev(v, "ws_mb", "self", stamp),
        priv_mb: ev(v, "priv_mb", "self", stamp),
        io_read_b: ev(v, "io_read_b", "self", stamp),
        io_write_b: ev(v, "io_write_b", "self", stamp),
        threads: ev(v, "threads", "self", stamp),
        ctx_switches: ev(v, "ctx_switches_s", "self", stamp),
    });
}

fn load_policy(v: &JVal, stamp: ClockStamp, s: &mut SessionData) {
    // The {"ac","dc"} pair holds envelopes; each side keeps its own stored
    // provenance/kind/reason, so AC vs DC comparisons are faithful.
    let side = |key: &str, which: &str| -> Telemetry<f64> {
        match v.get(key).and_then(|o| o.get(which)) {
            Some(o) => ev_inner(Some(o), "os", stamp),
            None => {
                Telemetry::unavailable(UnavailKind::NotSampled, "policy field missing", "os", stamp)
            }
        }
    };
    let pair = |key: &str| (side(key, "ac"), side(key, "dc"));
    let (cpu_min_ac, cpu_min_dc) = pair("cpu_min_pct");
    let (cpu_max_ac, cpu_max_dc) = pair("cpu_max_pct");
    let (epp_ac, epp_dc) = pair("epp");
    let (brightness_ac, brightness_dc) = pair("brightness_pct");
    let (display_timeout_ac, display_timeout_dc) = pair("display_timeout_s");
    let (sleep_timeout_ac, sleep_timeout_dc) = pair("sleep_timeout_s");
    let (hibernate_timeout_ac, hibernate_timeout_dc) = pair("hibernate_timeout_s");
    let (low_batt_ac, low_batt_dc) = pair("low_batt_pct");
    let (crit_batt_ac, crit_batt_dc) = pair("crit_batt_pct");
    let pol = Policy {
        scheme_name: v
            .get("scheme_name")
            .and_then(|n| n.as_str())
            .map(|x| x.to_string()),
        scheme_guid: v
            .get("active_scheme")
            .and_then(|f| f.get("v")?.as_str())
            .map(|x| x.to_string()),
        policy_snapshot_wall_ms: v
            .get("policy_snapshot_wall_ms")
            .and_then(|x| x.num())
            .map(|x| x as u64),
        timer_resolution: ev(v, "timer_resolution_ms", "os", stamp),
        cpu_min_ac,
        cpu_max_ac,
        cpu_min_dc,
        cpu_max_dc,
        epp_ac,
        epp_dc,
        brightness_ac,
        brightness_dc,
        display_timeout_ac,
        display_timeout_dc,
        sleep_timeout_ac,
        sleep_timeout_dc,
        hibernate_timeout_ac,
        hibernate_timeout_dc,
        low_batt_ac,
        low_batt_dc,
        crit_batt_ac,
        crit_batt_dc,
    };
    let t = stamp.mono_millis as f64 / 1000.0;
    // First occurrence stays the headline policy (compat); every
    // occurrence is recorded in history for change detection.
    if s.policy.scheme_name.is_none() {
        s.policy = pol.clone();
    }
    s.policy_history.push((t, pol));
}

/// One long-format CSV row: (mono_seconds, collector, metric, extra, value,
/// provenance, reason, quality).
pub type CsvRow = (f64, String, String, String, String, String, String, String);

/// CSV header line, without the trailing newline.
pub const CSV_HEADER: &str = "mono_s,collector,metric,extra,value,provenance,reason,quality";

/// One long-format evidence row with the full absence semantics retained.
///
/// Superset of [`CsvRow`]: it keeps the structured absence kind, wall clock and
/// a typed `Option` value, so the tidy machine-readable export can distinguish
/// `unavailable` / `not-sampled` / `stale` from an actual zero instead of
/// flattening absence into an empty cell. `None` is never rendered as `0`.
#[derive(Debug, Clone)]
pub struct RawRow {
    pub mono_s: f64,
    pub wall_ms: u64,
    pub collector: &'static str,
    pub metric: String,
    pub extra: String,
    /// `None` for an absent reading. Never coerced to `Some(0.0)`.
    pub value: Option<f64>,
    /// "measured" | "derived" | "estimated" | "unavailable"
    pub provenance: &'static str,
    /// "fresh" | "repeated" | "stale" | "error" | "unknown"
    pub quality: &'static str,
    /// "unsupported" | "transient" | "not-sampled" | "stale" when unavailable.
    pub unavailable_kind: Option<&'static str>,
    pub reason: String,
}

impl RawRow {
    pub fn is_unavailable(&self) -> bool {
        self.value.is_none()
    }

    /// Whether the stored evidence itself marks this row stale (as opposed to
    /// merely being an old sample): stale quality or a stale absence kind.
    pub fn is_stale(&self) -> bool {
        self.quality == "stale" || self.unavailable_kind == Some("stale")
    }

    /// Lossy projection to the legacy 8-column CSV contract.
    pub fn to_csv(&self) -> CsvRow {
        let value = self.value.map(|v| v.to_string()).unwrap_or_default();
        (
            self.mono_s,
            self.collector.to_string(),
            self.metric.clone(),
            self.extra.clone(),
            value,
            self.provenance.to_string(),
            self.reason.clone(),
            self.quality.to_string(),
        )
    }
}

/// RFC-4180-ish cell quoting. Shared by the streaming and whole-text paths so
/// they stay byte-identical.
pub fn format_csv_cell(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Render one row (with trailing newline) exactly as `csv_text` does.
pub fn format_csv_row(row: &CsvRow) -> String {
    let (t, c, m, e, v, p, r, q) = row;
    format!(
        "{t:.3},{c},{m},{},{v},{p},{},{q}\n",
        format_csv_cell(e),
        format_csv_cell(r)
    )
}

fn csv_row(t: f64, c: &'static str, m: &str, e: &str, v: &Telemetry<f64>) -> RawRow {
    let (value, reason) = match &v.value {
        Reading::Measured(x) | Reading::Derived(x) | Reading::Estimated(x) => {
            (Some(*x), String::new())
        }
        Reading::Unavailable(r) => (None, r.to_string()),
    };
    RawRow {
        mono_s: t,
        wall_ms: v.stamp.wall_millis,
        collector: c,
        metric: m.to_string(),
        extra: e.to_string(),
        value,
        provenance: v.provenance().as_str(),
        quality: v.quality.as_str(),
        unavailable_kind: v.unavail_kind.map(|k| k.as_str()),
        reason,
    }
}

/// Per-collector row sources, in the exact order `csv_rows` used to
/// concatenate before its stable sort. Each source is ascending in `t`, so the
/// merge below can use that order as the tie-break for equal timestamps and
/// stay byte-identical to the old `sort_by(total_cmp)` output.
fn csv_sources(s: &SessionData) -> Vec<Box<dyn Iterator<Item = RawRow> + '_>> {
    let mut sources: Vec<Box<dyn Iterator<Item = RawRow> + '_>> = Vec::new();
    sources.push(Box::new(s.battery.iter().flat_map(|b| {
        [
            csv_row(b.t, "battery", "discharge_w", "", &b.discharge),
            csv_row(b.t, "battery", "charge_w", "", &b.charge),
            csv_row(b.t, "battery", "remaining_wh", "", &b.remaining_wh),
            csv_row(b.t, "battery", "charge_pct", "", &b.pct),
        ]
    })));
    sources.push(Box::new(s.cpu.iter().flat_map(|c| {
        let mut rows = vec![
            csv_row(c.t, "cpu", "utility_pct", "", &c.utility),
            csv_row(c.t, "cpu", "c3_pct", "", &c.c3_pct),
            csv_row(c.t, "cpu", "idle_breaks_s", "", &c.idle_breaks),
            csv_row(c.t, "cpu", "pkg_derived_w", "", &c.pkg_derived_w),
            csv_row(c.t, "cpu", "pkg_power_w", "", &c.pkg_power_w),
            csv_row(c.t, "cpu", "ctx_switches_s", "", &c.ctx_switches),
        ];
        for (k, v) in &c.totals {
            rows.push(csv_row(c.t, "cpu", k, "", v));
        }
        for core in &c.cores {
            rows.push(csv_row(
                c.t,
                "cpu",
                "core.utility",
                &core.instance,
                &core.utility,
            ));
            rows.push(csv_row(
                c.t,
                "cpu",
                "core.performance",
                &core.instance,
                &core.performance,
            ));
            rows.push(csv_row(
                c.t,
                "cpu",
                "core.freq_mhz",
                &core.instance,
                &core.freq_mhz,
            ));
            rows.push(csv_row(
                c.t,
                "cpu",
                "core.parking",
                &core.instance,
                &core.parking,
            ));
        }
        rows.into_iter()
    })));
    sources.push(Box::new(s.battery_extra.iter().flat_map(|b| {
        [
            csv_row(b.t, "battery", "ac", "", &b.ac),
            csv_row(b.t, "battery", "rate_mw", "", &b.rate_mw),
            csv_row(b.t, "battery", "health", "", &b.health),
            csv_row(b.t, "battery", "temperature_raw", "", &b.temperature_raw),
        ]
    })));
    sources.push(Box::new(s.gpu.iter().flat_map(|g| {
        let mut rows = Vec::new();
        for a in &g.adapters {
            rows.push(csv_row(
                g.t,
                "gpu",
                "total_util_pct",
                &a.name,
                &a.total_util,
            ));
            rows.push(csv_row(
                g.t,
                "gpu",
                "mem_dedicated_mb",
                &a.name,
                &a.mem_dedicated_mb,
            ));
            rows.push(csv_row(
                g.t,
                "gpu",
                "mem_shared_mb",
                &a.name,
                &a.mem_shared_mb,
            ));
            for (m, v) in [
                ("util_3d", &a.util_3d),
                ("util_compute", &a.util_compute),
                ("util_decode", &a.util_decode),
                ("util_codec", &a.util_codec),
                ("util_copy", &a.util_copy),
                ("util_other", &a.util_other),
                ("engines_active", &a.engines_active),
                ("engines_seen", &a.engines_seen),
            ] {
                rows.push(csv_row(g.t, "gpu", m, &a.name, v));
            }
            for (pid, v) in &a.top_pids {
                rows.push(csv_row(
                    g.t,
                    "gpu",
                    "top_pid_util_pct",
                    &format!("{}:{pid}", a.name),
                    v,
                ));
            }
        }
        rows.into_iter()
    })));
    sources.push(Box::new(s.procs.iter().flat_map(|p| {
        p.top.iter().map(move |e| {
            csv_row(
                p.t,
                "proc",
                "cpu_pct",
                &format!("{}:{}", e.pid, e.name),
                &e.cpu,
            )
        })
    })));
    sources.push(Box::new(s.display.iter().flat_map(|d| {
        let mode = |v: Option<u32>| match v {
            Some(n) => Telemetry::measured(
                n as f64,
                "display",
                ClockStamp {
                    wall_millis: 0,
                    mono_millis: (d.t * 1000.0) as u64,
                },
            ),
            None => Telemetry::unavailable(
                UnavailKind::NotSampled,
                "display mode not reported",
                "display",
                ClockStamp {
                    wall_millis: 0,
                    mono_millis: (d.t * 1000.0) as u64,
                },
            ),
        };
        let mut rows = Vec::new();
        for (i, e) in d.displays.iter().enumerate() {
            let id = if e.name.is_empty() {
                i.to_string()
            } else {
                e.name.clone()
            };
            rows.push(csv_row(
                d.t,
                "display",
                "brightness_pct",
                &id,
                &e.brightness,
            ));
            rows.push(csv_row(d.t, "display", "width", &id, &mode(e.width)));
            rows.push(csv_row(d.t, "display", "height", &id, &mode(e.height)));
            rows.push(csv_row(d.t, "display", "freq_hz", &id, &mode(e.freq_hz)));
            rows.push(csv_row(d.t, "display", "bpp", &id, &mode(e.bpp)));
        }
        rows.into_iter()
    })));
    sources.push(Box::new(s.net.iter().flat_map(|n| {
        let mut rows = vec![
            csv_row(n.t, "net", "total_rx_bps", "", &n.total_rx),
            csv_row(n.t, "net", "total_tx_bps", "", &n.total_tx),
        ];
        for (inst, rx, tx) in &n.throughput {
            rows.push(csv_row(n.t, "net", "rx_bps", inst, rx));
            rows.push(csv_row(n.t, "net", "tx_bps", inst, tx));
        }
        for w in &n.wifi {
            rows.push(csv_row(
                n.t,
                "net",
                "wifi_signal_pct",
                &w.descr,
                &w.signal_pct,
            ));
        }
        rows.into_iter()
    })));
    sources.push(Box::new(s.storage.iter().flat_map(|d| {
        [
            csv_row(d.t, "storage", "disk_time_pct", "_total", &d.disk_time),
            csv_row(d.t, "storage", "read_bps", "_total", &d.read_bps),
            csv_row(d.t, "storage", "write_bps", "_total", &d.write_bps),
        ]
    })));
    sources.push(Box::new(s.usb.iter().flat_map(|u| {
        u.classes
            .iter()
            .map(move |c| csv_row(u.t, "usb", "device_count", &c.class, &c.evidence))
    })));
    sources.push(Box::new(s.selfmon.iter().flat_map(|m| {
        [
            csv_row(m.t, "self", "cpu_pct", "", &m.cpu_pct),
            csv_row(m.t, "self", "ws_mb", "", &m.ws_mb),
            csv_row(m.t, "self", "priv_mb", "", &m.priv_mb),
            csv_row(m.t, "self", "io_read_b", "", &m.io_read_b),
            csv_row(m.t, "self", "io_write_b", "", &m.io_write_b),
            csv_row(m.t, "self", "threads", "", &m.threads),
            csv_row(m.t, "self", "ctx_switches_s", "", &m.ctx_switches),
        ]
    })));
    sources
}

struct CsvMergeItem {
    t: f64,
    block: usize,
    seq: u64,
    row: RawRow,
}

impl PartialEq for CsvMergeItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for CsvMergeItem {}
impl PartialOrd for CsvMergeItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CsvMergeItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed for a min-heap; ties fall back to source-block order and
        // then emission order inside the block, which is exactly the stable
        // sort's tie order.
        other
            .t
            .total_cmp(&self.t)
            .then_with(|| other.block.cmp(&self.block))
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// Lazy, time-sorted raw evidence rows. Each source is already ascending in
/// `t`, so a k-way merge reproduces the old global `sort_by(total_cmp)` order
/// (including equal-timestamp ties) without materializing every row.
fn raw_rows_iter(s: &SessionData) -> impl Iterator<Item = RawRow> + '_ {
    let mut sources = csv_sources(s);
    let mut heap: BinaryHeap<CsvMergeItem> = BinaryHeap::new();
    for (block, src) in sources.iter_mut().enumerate() {
        if let Some(row) = src.next() {
            heap.push(CsvMergeItem {
                t: row.mono_s,
                block,
                seq: 0,
                row,
            });
        }
    }
    let mut next_seq = vec![1u64; sources.len()];
    std::iter::from_fn(move || {
        let item = heap.pop()?;
        if let Some(row) = sources[item.block].next() {
            let seq = next_seq[item.block];
            next_seq[item.block] += 1;
            heap.push(CsvMergeItem {
                t: row.mono_s,
                block: item.block,
                seq,
                row,
            });
        }
        Some(item.row)
    })
}

/// Time-sorted rich rows for the tidy machine-readable export. Unlike the
/// legacy 8-column CSV this keeps the structured absence kind and wall clock so
/// provenance and stale/unavailable semantics survive export unchanged.
pub fn export_rows_iter(s: &SessionData) -> impl Iterator<Item = RawRow> + '_ {
    raw_rows_iter(s)
}

/// Lazy, time-sorted legacy CSV rows (lossy 8-column projection).
pub fn csv_rows_iter(s: &SessionData) -> impl Iterator<Item = CsvRow> + '_ {
    raw_rows_iter(s).map(|r| r.to_csv())
}

/// Long-format CSV rows for manual analysis (Excel pivot-friendly):
/// (mono_seconds, collector, metric, extra, value, provenance, reason,
/// quality). Provenance/reason/quality come from the STORED evidence —
/// never reconstructed from metric names. Unavailable rows ARE emitted
/// (with empty value) so absence stays distinguishable from missing data.
pub fn csv_rows(s: &SessionData) -> Vec<CsvRow> {
    csv_rows_iter(s).collect()
}

pub fn csv_text(s: &SessionData) -> String {
    let mut o = String::from(CSV_HEADER);
    o.push('\n');
    for row in csv_rows_iter(s) {
        o.push_str(&format_csv_row(&row));
    }
    o
}

/// Load a full session file (JSONL text) into typed series.
pub fn load_session(label: &str, text: &str) -> Result<SessionData, String> {
    let mut s = SessionData {
        label: label.to_string(),
        ..Default::default()
    };
    for (ln, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v = parse(line).map_err(|e| format!("line {}: {e}", ln + 1))?;
        // Wall base: first nonzero wall clock seen; archive wall times
        // derive from it (wall_base_ms + mono_ms).
        if s.wall_base_ms == 0 {
            let w = wall_ms(&v);
            if w > 0 {
                s.wall_base_ms = w;
            }
        }
        let stamp = stamp_of(&v);
        if v.get("collector").and_then(|c| c.as_str()) == Some("battery") {
            load_battery(&v, stamp, &mut s);
        } else if v.get("collector").and_then(|c| c.as_str()) == Some("cpu") {
            load_cpu(&v, stamp, &mut s);
        } else if v.get("collector").and_then(|c| c.as_str()) == Some("gpu") {
            load_gpu(&v, stamp, &mut s);
        } else if v.get("collector").and_then(|c| c.as_str()) == Some("proc") {
            load_procs(&v, stamp, &mut s);
        } else if v.get("collector").and_then(|c| c.as_str()) == Some("display") {
            load_display(&v, stamp, &mut s);
        } else if v.get("collector").and_then(|c| c.as_str()) == Some("net") {
            load_net(&v, stamp, &mut s);
        } else if v.get("collector").and_then(|c| c.as_str()) == Some("storage") {
            load_storage(&v, stamp, &mut s);
        } else if v.get("collector").and_then(|c| c.as_str()) == Some("usb") {
            load_usb(&v, stamp, &mut s);
        } else if v.get("collector").and_then(|c| c.as_str()) == Some("self") {
            load_selfmon(&v, stamp, &mut s);
        } else if v.get("collector").and_then(|c| c.as_str()) == Some("os_power") {
            load_policy(&v, stamp, &mut s);
        } else if v.get("type").and_then(|t| t.as_str()) == Some("event") {
            s.events.push(EventRec {
                wall_ms: wall_ms(&v),
                mono_ms: stamp.mono_millis,
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
        } else if v.get("type").and_then(|t| t.as_str()) == Some("session_footer") {
            s.footer = v.get("summary").cloned();
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{Provenance, SampleQuality, Telemetry, UnavailKind};

    fn sample_session() -> &'static str {
        "{\"type\":\"session_header\",\"wall_ms\":1}\n\
         {\"collector\":\"battery\",\"wall_ms\":1,\"mono_ms\":0,\"discharge_w\":{\"v\":5.7,\"p\":\"measured\"},\"charge_w\":{\"v\":null,\"p\":\"unavailable\",\"reason\":\"x\"},\"remaining_mwh\":{\"v\":20000,\"p\":\"measured\"},\"full_charge_mwh\":{\"v\":37090,\"p\":\"measured\"},\"charge_pct\":{\"v\":55,\"p\":\"measured\"}}\n\
         {\"collector\":\"battery\",\"wall_ms\":2,\"mono_ms\":1000,\"discharge_w\":{\"v\":11.4,\"p\":\"measured\"},\"remaining_mwh\":{\"v\":19990,\"p\":\"measured\"}}\n\
         {\"collector\":\"cpu\",\"wall_ms\":1,\"mono_ms\":0,\"totals\":{\"utility\":{\"v\":4.0,\"p\":\"measured\"},\"c3_pct\":{\"v\":90.0,\"p\":\"measured\"}},\"energy\":{\"pkg_derived_w\":{\"v\":2.1,\"p\":\"estimated\"}}}\n\
         {\"collector\":\"gpu\",\"wall_ms\":1,\"mono_ms\":0,\"adapters\":[{\"name\":\"AMD\",\"discrete\":false,\"total_util_pct\":0.2}]}\n\
         {\"collector\":\"proc\",\"wall_ms\":1,\"mono_ms\":0,\"top\":[{\"pid\":1,\"name\":\"a.exe\",\"cpu_pct\":{\"v\":0.1,\"p\":\"derived\"}}]}\n\
         {\"collector\":\"display\",\"wall_ms\":1,\"mono_ms\":0,\"displays\":[{\"brightness_pct\":{\"v\":40,\"p\":\"measured\"}}]}\n\
         {\"collector\":\"os_power\",\"wall_ms\":1,\"mono_ms\":0,\"scheme_name\":\"Balanced\",\"cpu_min_pct\":{\"ac\":{\"v\":5,\"p\":\"measured\"},\"dc\":{\"v\":5,\"p\":\"measured\"}}}\n\
         {\"type\":\"event\",\"wall_ms\":1,\"mono_ms\":0,\"kind\":\"anomaly_hint\",\"detail\":\"x\"}\n\
         {\"type\":\"session_footer\",\"wall_ms\":2,\"lines\":9,\"summary\":{\"discharge_wh\":0.01}}\n"
    }

    #[test]
    fn loads_typed_series() {
        let s = load_session("t", sample_session()).unwrap();
        assert_eq!(s.battery.len(), 2);
        assert_eq!(s.battery[0].discharge.val(), Some(5.7));
        assert_eq!(s.battery[0].discharge.provenance(), Provenance::Measured);
        assert_eq!(s.battery[0].discharge.source, "battery");
        assert_eq!(s.battery[0].discharge.stamp.mono_millis, 0);
        // Unavailable keeps kind/reason/quality instead of vanishing.
        assert_eq!(s.battery[0].charge.val(), None);
        assert_eq!(s.battery[0].charge.provenance(), Provenance::Unavailable);
        assert_eq!(s.battery[0].charge.value.reason(), Some("x"));
        assert_eq!(
            s.battery[0].charge.unavail_kind,
            Some(UnavailKind::NotSampled)
        );
        assert_eq!(s.battery[0].charge.quality, SampleQuality::Unknown);
        assert!((s.battery[0].remaining_wh.val().unwrap() - 20.0).abs() < 1e-9);
        assert_eq!(s.battery_meta.full_mwh, Some(37090.0));
        assert_eq!(s.cpu[0].c3_pct.val(), Some(90.0));
        assert_eq!(s.cpu[0].pkg_derived_w.val(), Some(2.1));
        assert_eq!(s.cpu[0].pkg_derived_w.provenance(), Provenance::Estimated);
        assert_eq!(s.gpu[0].adapters[0].name, "AMD");
        assert!(!s.gpu[0].adapters[0].discrete);
        assert_eq!(s.procs[0].top[0].pid, 1);
        assert_eq!(s.display[0].brightness.val(), Some(40.0));
        assert_eq!(s.policy.scheme_name.as_deref(), Some("Balanced"));
        assert_eq!(s.policy.cpu_min_dc.val(), Some(5.0));
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].mono_ms, 0);
        assert_eq!(
            s.footer
                .as_ref()
                .unwrap()
                .get("discharge_wh")
                .unwrap()
                .num(),
            Some(0.01)
        );
    }

    #[test]
    fn corrupt_line_fails_with_number() {
        let bad = "{\"collector\":\"battery\"}\nNOT JSON\n";
        let e = load_session("t", bad).unwrap_err();
        assert!(e.contains("line 2"), "{e}");
    }

    #[test]
    fn empty_session_loads() {
        let s = load_session("t", "\n").unwrap();
        assert!(s.battery.is_empty());
    }

    #[test]
    fn fixture_ac_transition() {
        let s = load_session("ac", include_str!("../fixtures/ac-transition.jsonl")).unwrap();
        assert_eq!(s.battery.len(), 20);
        assert!(s.battery[0].discharge.val().is_some());
        assert!(s.battery[0].charge.val().is_none());
        assert!(s.battery[15].discharge.val().is_none());
        assert!(s.battery[15].charge.val().is_some());
    }

    #[test]
    fn fixture_unavailable_stays_absent() {
        let s = load_session("u", include_str!("../fixtures/unavailable.jsonl")).unwrap();
        assert!(
            s.battery
                .iter()
                .all(|p| p.discharge.val().is_none() && p.remaining_wh.val().is_none())
        );
        // ...but absence is classified, not blank.
        assert!(s.battery[0].discharge.unavail_kind.is_some());
    }

    #[test]
    fn fixture_policy_first_wins_current_behavior() {
        // Headline policy stays the first snapshot; every snapshot is also
        // recorded in history for change detection.
        let s = load_session("p", include_str!("../fixtures/policy-change.jsonl")).unwrap();
        assert_eq!(s.policy.scheme_name.as_deref(), Some("High performance"));
        // History records every snapshot for change detection.
        assert_eq!(s.policy_history.len(), 10);
        assert_eq!(
            s.policy_history[0].1.scheme_name.as_deref(),
            Some("High performance")
        );
        assert_eq!(
            s.policy_history[9].1.scheme_name.as_deref(),
            Some("Balanced")
        );
        assert_eq!(s.policy_history[9].1.brightness_dc.val(), Some(40.0));
    }

    #[test]
    fn fixture_sleep_gap_preserves_timestamps() {
        let s = load_session("g", include_str!("../fixtures/sleep-gap.jsonl")).unwrap();
        assert_eq!(s.battery.len(), 20);
        assert!((s.battery[9].t - 9.0).abs() < 1e-9);
        assert!((s.battery[10].t - 400.0).abs() < 1e-9);
    }

    #[test]
    fn fixture_collector_failure_loads_gaps() {
        let s = load_session("f", include_str!("../fixtures/collector-failure.jsonl")).unwrap();
        assert_eq!(s.battery.len(), 10);
        // 7 of 10 ticks have CPU (every 3rd tick errors).
        assert_eq!(s.cpu.len(), 7);
        assert!(s.events.iter().any(|e| e.kind == "collector_error"));
    }

    #[test]
    fn net_storage_usb_load() {
        let text = "{\"collector\":\"net\",\"mono_ms\":0,\"total_rx_bps\":100.0,\"total_tx_bps\":50.0}\n\
            {\"collector\":\"storage\",\"mono_ms\":0,\"disks\":[{\"instance\":\"0 C:\",\"total\":false,\"disk_time_pct\":{\"v\":3.0,\"p\":\"measured\"},\"read_bps\":{\"v\":1000.0,\"p\":\"measured\"},\"write_bps\":{\"v\":2000.0,\"p\":\"measured\"}},{\"instance\":\"_Total\",\"total\":true,\"disk_time_pct\":{\"v\":3.0,\"p\":\"measured\"},\"read_bps\":{\"v\":1000.0,\"p\":\"measured\"},\"write_bps\":{\"v\":2000.0,\"p\":\"measured\"}}]}\n\
            {\"collector\":\"usb\",\"mono_ms\":0,\"groups\":[{\"class\":\"usb\",\"devices\":[{\"instance\":\"A\"},{\"instance\":\"B\"}]},{\"class\":\"hid\",\"devices\":[]}]}\n";
        let s = load_session("t", text).unwrap();
        assert_eq!(s.net[0].total_rx.val(), Some(100.0));
        // Plain numbers take the compatibility path: measured, freshness unknown.
        assert_eq!(s.net[0].total_rx.provenance(), Provenance::Measured);
        assert_eq!(s.net[0].total_rx.quality, SampleQuality::Unknown);
        assert_eq!(s.storage[0].write_bps.val(), Some(2000.0));
        assert_eq!(
            s.usb[0].counts(),
            vec![("usb".to_string(), 2), ("hid".to_string(), 0)]
        );
        // No count envelope: the count derived from this line's own
        // devices[] evidence is DERIVED, never promoted to Measured.
        assert_eq!(
            s.usb[0].classes[0].evidence.provenance(),
            Provenance::Derived
        );
        assert_eq!(s.usb[0].classes[0].evidence.quality, SampleQuality::Fresh);
        assert_eq!(s.usb[0].classes[0].evidence.val(), Some(2.0));
    }

    #[test]
    fn usb_count_without_device_evidence_is_unavailable() {
        // A group with neither a count envelope nor devices[] evidence has
        // no measurement basis: the count must be Unavailable, not 0/Measured.
        let text = "{\"collector\":\"usb\",\"mono_ms\":0,\"groups\":[{\"class\":\"camera\"}]}\n";
        let s = load_session("t", text).unwrap();
        let e = &s.usb[0].classes[0].evidence;
        assert_eq!(e.provenance(), Provenance::Unavailable);
        assert_eq!(e.val(), None);
        assert_eq!(e.value.reason(), Some("no device evidence in usb group"));
        assert_eq!(e.unavail_kind, Some(UnavailKind::NotSampled));
        // CSV keeps absence distinguishable instead of exporting a fake 0.
        let csv = csv_text(&s);
        assert!(csv.lines().any(|l| {
            l.contains(
                "usb,device_count,camera,,unavailable,no device evidence in usb group,unknown",
            )
        }));
    }

    #[test]
    fn usb_count_envelope_carries_evidence() {
        // A per-group "count" envelope is honored like any other envelope.
        let text = "{\"collector\":\"usb\",\"wall_ms\":5,\"mono_ms\":4000,\"groups\":[\
            {\"class\":\"usb\",\"devices\":[{\"instance\":\"A\"},{\"instance\":\"B\"}],\
            \"count\":{\"v\":2,\"p\":\"derived\",\"q\":1}}]}\n";
        let s = load_session("t", text).unwrap();
        assert_eq!(s.usb[0].counts(), vec![("usb".to_string(), 2)]);
        let e = &s.usb[0].classes[0].evidence;
        assert_eq!(e.provenance(), Provenance::Derived);
        assert_eq!(e.quality, SampleQuality::RepeatedCached);
        assert_eq!(e.source, "usb");
        assert_eq!(e.stamp.mono_millis, 4000);
        // CSV exports the stored evidence, never hard-coded prov/quality.
        let csv = csv_text(&s);
        assert!(
            csv.lines()
                .any(|l| l.contains("usb,device_count,usb,2,derived,,repeated"))
        );
    }

    #[test]
    fn usb_csv_uses_stored_evidence() {
        let stamp = crate::telemetry::ClockStamp {
            wall_millis: 0,
            mono_millis: 0,
        };
        let mut s = SessionData::default();
        s.usb.push(UsbPoint {
            t: 0.0,
            classes: vec![UsbClassCount {
                class: "hid".to_string(),
                count: 0,
                evidence: Telemetry::unavailable(
                    UnavailKind::NotSampled,
                    "no hid enumeration",
                    "usb",
                    stamp,
                ),
            }],
            ..Default::default()
        });
        let csv = csv_text(&s);
        assert!(csv.lines().any(|l| {
            l.contains("usb,device_count,hid,,unavailable,no hid enumeration,unknown")
        }));
    }

    #[test]
    fn battery_static_provenance_loads() {
        let text = "{\"collector\":\"battery\",\"wall_ms\":1,\"mono_ms\":0,\
            \"full_charge_mwh\":{\"v\":37090,\"p\":\"estimated\"},\"design_mwh\":{\"v\":42000,\"p\":\"measured\"},\
            \"batteries\":[{\"index\":0,\"designed_mwh\":{\"v\":42000,\"p\":\"measured\"},\
            \"full_mwh\":{\"v\":null,\"p\":\"unavailable\",\"k\":0,\"q\":4,\"reason\":\"no full reading\"}}]}\n";
        let s = load_session("t", text).unwrap();
        assert_eq!(s.battery_meta.full_prov, "estimated");
        assert_eq!(s.battery_meta.design_prov, "measured");
        assert_eq!(s.battery_meta.batteries[0].design_prov, "measured");
        assert_eq!(s.battery_meta.batteries[0].full_prov, "unavailable");
        // Missing envelopes never fabricate provenance.
        let s2 = load_session(
            "t",
            "{\"collector\":\"battery\",\"wall_ms\":1,\"mono_ms\":0}\n",
        )
        .unwrap();
        assert_eq!(s2.battery_meta.full_prov, "unavailable");
        assert_eq!(s2.battery_meta.design_prov, "unavailable");
    }

    #[test]
    fn proc_exit_timestamp_loads() {
        let text = "{\"collector\":\"proc\",\"wall_ms\":1,\"mono_ms\":0,\"top\":[\
            {\"pid\":7,\"ppid\":1,\"name\":\"x.exe\",\"exit_unix_ms\":12345,\
            \"cpu_pct\":{\"v\":0.1,\"p\":\"derived\"}},\
            {\"pid\":8,\"ppid\":1,\"name\":\"y.exe\",\"cpu_pct\":{\"v\":0.2,\"p\":\"derived\"}}]}\n";
        let s = load_session("t", text).unwrap();
        assert_eq!(s.procs[0].top[0].exit_unix_ms, Some(12345));
        // Absent key means running-or-unknown, never an implicit exit.
        assert_eq!(s.procs[0].top[1].exit_unix_ms, None);
    }

    #[test]
    fn csv_long_format() {
        use crate::telemetry::ClockStamp;
        let stamp = ClockStamp {
            wall_millis: 0,
            mono_millis: 1000,
        };
        let mut s = SessionData::default();
        s.battery.push(BatteryPoint {
            t: 1.0,
            discharge: Telemetry::measured(6.4, "battery", stamp),
            charge: Telemetry::unavailable(
                UnavailKind::Unsupported,
                "not charging",
                "battery",
                stamp,
            ),
            ..Default::default()
        });
        s.cpu.push(CpuPoint {
            t: 1.0,
            utility: Telemetry::measured(5.0, "cpu", stamp),
            pkg_derived_w: Telemetry::estimated(2.0, "cpu", stamp),
            ..Default::default()
        });
        s.gpu.push(GpuPoint {
            t: 1.0,
            adapters: vec![GpuAdapterPoint {
                name: "AMD".to_string(),
                total_util: Telemetry::derived(3.0, "gpu", stamp),
                ..Default::default()
            }],
            ..Default::default()
        });
        let text = csv_text(&s);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "mono_s,collector,metric,extra,value,provenance,reason,quality"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("battery,discharge_w,,6.4,measured,,fresh"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("cpu,pkg_derived_w,,2,estimated,,fresh"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("gpu,total_util_pct,AMD,3,derived,,fresh"))
        );
        // Unavailable rows exist with empty value and preserved reason.
        assert!(
            lines
                .iter()
                .any(|l| l.contains("battery,charge_w,,,unavailable,not charging,unknown"))
        );
    }

    /// Phase-2 gate: a session containing all four provenance types plus
    /// both absence kinds must survive load -> export with value,
    /// provenance, kind, reason, and quality intact. Present values with
    /// an unknown kind must be demoted, never defaulted to measured.
    #[test]
    fn provenance_gate_all_kinds_round_trip() {
        let text = "{\"collector\":\"cpu\",\"wall_ms\":9,\"mono_ms\":7000,\
            \"totals\":{\"utility\":{\"v\":11.0,\"p\":\"measured\",\"k\":2,\"q\":0},\
            \"c3_pct\":{\"v\":80.0,\"p\":\"derived\",\"k\":2,\"q\":1}},\
            \"energy\":{\"pkg_derived_w\":{\"v\":3.3,\"p\":\"estimated\",\"k\":2,\"q\":0}}}\n\
            {\"collector\":\"cpu\",\"wall_ms\":10,\"mono_ms\":8000,\
            \"totals\":{\"utility\":{\"v\":null,\"p\":\"unavailable\",\"k\":0,\"q\":4,\"reason\":\"no sensor here\"},\
            \"c3_pct\":{\"v\":null,\"p\":\"unavailable\",\"k\":1,\"q\":3,\"reason\":\"driver hiccup\"}},\
            \"energy\":{\"pkg_derived_w\":{\"v\":9.9,\"p\":\"mystery\"}}}\n";
        let s = load_session("gate", text).unwrap();
        assert_eq!(s.cpu.len(), 2);
        let u0 = &s.cpu[0].utility;
        assert_eq!(u0.val(), Some(11.0));
        assert_eq!(u0.provenance(), Provenance::Measured);
        assert_eq!(u0.quality, SampleQuality::Fresh);
        assert_eq!(u0.source, "cpu");
        assert_eq!(u0.stamp.mono_millis, 7000);
        assert_eq!(s.cpu[0].c3_pct.provenance(), Provenance::Derived);
        assert_eq!(s.cpu[0].c3_pct.quality, SampleQuality::RepeatedCached);
        assert_eq!(s.cpu[0].pkg_derived_w.provenance(), Provenance::Estimated);
        let u1 = &s.cpu[1].utility;
        assert_eq!(u1.val(), None);
        assert_eq!(u1.unavail_kind, Some(UnavailKind::Unsupported));
        assert_eq!(u1.value.reason(), Some("no sensor here"));
        assert_eq!(
            s.cpu[1].c3_pct.unavail_kind,
            Some(UnavailKind::TransientError)
        );
        assert_eq!(s.cpu[1].c3_pct.quality, SampleQuality::Error);
        // Present value, unknown kind: demoted, value dropped, reason kept.
        let p1 = &s.cpu[1].pkg_derived_w;
        assert_eq!(p1.val(), None);
        assert_eq!(p1.provenance(), Provenance::Unavailable);
        // Export preserves all of it.
        let csv = csv_text(&s);
        assert!(
            csv.lines()
                .any(|l| l.contains("cpu,utility_pct,,11,measured,,fresh"))
        );
        assert!(
            csv.lines()
                .any(|l| l.contains("cpu,c3_pct,,80,derived,,repeated"))
        );
        assert!(
            csv.lines()
                .any(|l| l.contains("cpu,utility_pct,,,unavailable,no sensor here,unknown"))
        );
        assert!(
            csv.lines()
                .any(|l| l.contains("cpu,c3_pct,,,unavailable,driver hiccup,error"))
        );
        assert!(csv.lines().any(|l| {
            l.contains("cpu,pkg_derived_w,")
                && l.contains("unavailable,present value with unknown provenance kind,unknown")
        }));
    }

    /// Byte-for-byte regression guard for the streaming CSV refactor: this is
    /// the exact output the pre-refactor `csv_rows` + `sort_by(total_cmp)`
    /// builder produced for `sample_session` (interleaved collectors and
    /// equal timestamps exercise the merge tie order).
    #[test]
    fn csv_text_golden_matches_pre_stream_builder() {
        let s = load_session("t", sample_session()).unwrap();
        let expected = "\
mono_s,collector,metric,extra,value,provenance,reason,quality
0.000,battery,discharge_w,,5.7,measured,,fresh
0.000,battery,charge_w,,,unavailable,x,unknown
0.000,battery,remaining_wh,,20,derived,,fresh
0.000,battery,charge_pct,,55,measured,,fresh
0.000,cpu,utility_pct,,4,measured,,fresh
0.000,cpu,c3_pct,,90,measured,,fresh
0.000,cpu,idle_breaks_s,,,unavailable,no reason recorded,unknown
0.000,cpu,pkg_derived_w,,2.1,estimated,,fresh
0.000,cpu,pkg_power_w,,,unavailable,no reason recorded,unknown
0.000,cpu,ctx_switches_s,,,unavailable,no reason recorded,unknown
0.000,battery,ac,,,unavailable,field missing,unknown
0.000,battery,rate_mw,,,unavailable,no reason recorded,unknown
0.000,battery,health,,,unavailable,no reason recorded,unknown
0.000,battery,temperature_raw,,,unavailable,no reason recorded,unknown
0.000,gpu,total_util_pct,AMD,0.2,measured,,unknown
0.000,gpu,mem_dedicated_mb,AMD,,unavailable,no reason recorded,unknown
0.000,gpu,mem_shared_mb,AMD,,unavailable,no reason recorded,unknown
0.000,gpu,util_3d,AMD,,unavailable,no reason recorded,unknown
0.000,gpu,util_compute,AMD,,unavailable,no reason recorded,unknown
0.000,gpu,util_decode,AMD,,unavailable,no reason recorded,unknown
0.000,gpu,util_codec,AMD,,unavailable,no reason recorded,unknown
0.000,gpu,util_copy,AMD,,unavailable,no reason recorded,unknown
0.000,gpu,util_other,AMD,,unavailable,no reason recorded,unknown
0.000,gpu,engines_active,AMD,,unavailable,no reason recorded,unknown
0.000,gpu,engines_seen,AMD,,unavailable,no reason recorded,unknown
0.000,proc,cpu_pct,1:a.exe,0.1,derived,,fresh
0.000,display,brightness_pct,?,40,measured,,fresh
0.000,display,width,?,,unavailable,display mode not reported,unknown
0.000,display,height,?,,unavailable,display mode not reported,unknown
0.000,display,freq_hz,?,,unavailable,display mode not reported,unknown
0.000,display,bpp,?,,unavailable,display mode not reported,unknown
1.000,battery,discharge_w,,11.4,measured,,fresh
1.000,battery,charge_w,,,unavailable,no reason recorded,unknown
1.000,battery,remaining_wh,,19.99,derived,,fresh
1.000,battery,charge_pct,,,unavailable,no reason recorded,unknown
1.000,battery,ac,,,unavailable,field missing,unknown
1.000,battery,rate_mw,,,unavailable,no reason recorded,unknown
1.000,battery,health,,,unavailable,no reason recorded,unknown
1.000,battery,temperature_raw,,,unavailable,no reason recorded,unknown
";
        assert_eq!(csv_text(&s), expected);
        // The lazy iterator (headerless) and the materialized rows agree.
        let body = expected.split_once('\n').map(|(_, b)| b).unwrap();
        assert_eq!(
            csv_rows_iter(&s)
                .map(|r| format_csv_row(&r))
                .collect::<String>(),
            body
        );
        assert_eq!(csv_rows(&s), csv_rows_iter(&s).collect::<Vec<_>>());
    }

    #[test]
    fn csv_rows_iter_is_time_sorted_with_block_tie_order() {
        use crate::telemetry::ClockStamp;
        let stamp = ClockStamp {
            wall_millis: 0,
            mono_millis: 0,
        };
        let mut s = SessionData::default();
        s.battery.push(BatteryPoint {
            t: 0.0,
            discharge: Telemetry::measured(1.0, "battery", stamp),
            ..Default::default()
        });
        s.battery.push(BatteryPoint {
            t: 2.0,
            discharge: Telemetry::measured(2.0, "battery", stamp),
            ..Default::default()
        });
        s.cpu.push(CpuPoint {
            t: 1.0,
            utility: Telemetry::measured(3.0, "cpu", stamp),
            ..Default::default()
        });
        s.gpu.push(GpuPoint {
            t: 1.0,
            adapters: vec![GpuAdapterPoint {
                name: "AMD".to_string(),
                total_util: Telemetry::derived(4.0, "gpu", stamp),
                ..Default::default()
            }],
            ..Default::default()
        });
        let rows: Vec<CsvRow> = csv_rows_iter(&s).collect();
        let ts: Vec<f64> = rows.iter().map(|r| r.0).collect();
        assert!(
            ts.windows(2).all(|w| w[0] <= w[1]),
            "rows not time sorted: {ts:?}"
        );
        // Equal timestamps keep collector block order: every cpu row precedes
        // every gpu row (the cpu point contributes six rows first).
        let t1: Vec<&str> = rows
            .iter()
            .filter(|r| r.0 == 1.0)
            .map(|r| r.1.as_str())
            .collect();
        let first_gpu = t1
            .iter()
            .position(|c| *c == "gpu")
            .expect("gpu row missing");
        assert!(
            t1[..first_gpu].iter().all(|c| *c == "cpu"),
            "cpu block split: {t1:?}"
        );
        assert!(
            t1[first_gpu..].iter().all(|c| *c == "gpu"),
            "gpu block split: {t1:?}"
        );
    }

    #[test]
    fn net_topology_staleness_survives_loading() {
        let text = "{\"type\":\"session_header\",\"wall_ms\":1}\n\
             {\"collector\":\"net\",\"wall_ms\":1,\"mono_ms\":0,\
             \"total_rx_bps\":{\"v\":1.0,\"p\":\"derived\"},\
             \"throughput\":[],\"adapters\":[],\"wifi\":[],\"changes\":[],\
             \"topology_stale\":true,\
             \"topology_error\":\"no Network Interface counters could be added\"}\n\
             {\"type\":\"session_footer\",\"wall_ms\":2,\"lines\":3,\"summary\":{}}\n";
        let s = load_session("t", text).unwrap();
        assert_eq!(s.net.len(), 1);
        assert!(
            s.net[0].topology_stale,
            "rebuild-failure staleness must survive to SessionData"
        );
        // Absent flag defaults to false, not fabricated stale.
        let clean = "{\"type\":\"session_header\",\"wall_ms\":1}\n\
             {\"collector\":\"net\",\"wall_ms\":1,\"mono_ms\":0,\
             \"total_rx_bps\":{\"v\":1.0,\"p\":\"derived\"},\
             \"throughput\":[],\"adapters\":[],\"wifi\":[],\"changes\":[]}\n\
             {\"type\":\"session_footer\",\"wall_ms\":2,\"lines\":3,\"summary\":{}}\n";
        assert!(!load_session("t", clean).unwrap().net[0].topology_stale);
    }

    /// Absence of a presentation field must stay absent; a reported zero
    /// must stay a reported zero. Legacy hand-written JSON without the
    /// fields must not fabricate `Measured(0)` evidence.
    #[test]
    fn presentation_fields_distinguish_missing_from_zero() {
        let text = "{\"type\":\"session_header\",\"wall_ms\":1}\n\
             {\"collector\":\"display\",\"wall_ms\":1,\"mono_ms\":0,\"displays\":[\
             {\"name\":\"A\",\"brightness_pct\":{\"v\":10,\"p\":\"measured\"}},\
             {\"name\":\"B\",\"width\":0,\"height\":1080,\"freq_hz\":60,\"bpp\":32,\
             \"brightness_pct\":{\"v\":10,\"p\":\"measured\"}}],\
             \"unmatched_sensors\":[],\"hdr_targets\":[],\"changes\":[]}\n\
             {\"collector\":\"net\",\"wall_ms\":1,\"mono_ms\":0,\
             \"total_rx_bps\":{\"v\":1.0,\"p\":\"derived\"},\
             \"adapters\":[{\"alias\":\"A\",\"descr\":\"d\",\"class\":\"wifi\",\
             \"iftype\":\"wifi\",\"oper\":1,\"media\":\"Connected\",\"admin_up\":true,\
             \"tx_mbps\":1.0,\"rx_mbps\":1.0,\"in_octets\":1,\"out_octets\":1},\
             {\"alias\":\"B\",\"descr\":\"d\",\"class\":\"wifi\",\"iftype\":\"wifi\",\
             \"admin_up\":false,\"tx_mbps\":0.0,\"rx_mbps\":0.0,\"in_octets\":0,\"out_octets\":0}],\
             \"throughput\":[],\"wifi\":[{\"descr\":\"w\",\"ssid\":\"s\"}],\"changes\":[]}\n\
             {\"type\":\"session_footer\",\"wall_ms\":2,\"lines\":3,\"summary\":{}}\n";
        let s = load_session("t", text).unwrap();
        let d = &s.display[0].displays;
        // A: fields omitted -> None (never measured zero).
        assert_eq!(
            (d[0].width, d[0].height, d[0].freq_hz, d[0].bpp),
            (None, None, None, None)
        );
        // B: explicitly reported values survive, including a genuine width 0.
        assert_eq!(
            (d[1].width, d[1].height, d[1].freq_hz, d[1].bpp),
            (Some(0), Some(1080), Some(60), Some(32))
        );
        let a = &s.net[0].adapters;
        assert_eq!((a[0].oper, a[0].media), (Some(1.0), Some(1.0)));
        assert_eq!((a[1].oper, a[1].media), (None, None));
        assert_eq!(s.net[0].wifi[0].state, None);
    }

    /// Process-observation incompleteness is evidence: a tick that could not
    /// inspect every process must stay distinguishable from "no relevant
    /// processes", and legacy records with no coverage metadata must be
    /// UNKNOWN, never silently "complete".
    #[test]
    fn proc_coverage_uncertainty_survives_loading() {
        let text = "{\"type\":\"session_header\",\"wall_ms\":1}\n\
             {\"collector\":\"proc\",\"wall_ms\":1,\"mono_ms\":0,\"total_procs\":150,\
              \"total_threads\":400,\"inaccessible\":2,\"truncated\":true,\"top\":[]}\n\
             {\"collector\":\"proc\",\"wall_ms\":2,\"mono_ms\":1000,\"total_procs\":150,\
              \"total_threads\":401,\"inaccessible\":0,\"truncated\":false,\"top\":[]}\n\
             {\"collector\":\"proc\",\"wall_ms\":3,\"mono_ms\":2000,\"total_procs\":150,\"top\":[]}\n\
             {\"type\":\"session_footer\",\"wall_ms\":4,\"lines\":5,\"summary\":{}}\n";
        let s = load_session("t", text).unwrap();
        // The incomplete tick is retained verbatim, not zeroed.
        assert_eq!(s.procs[0].total_threads, Some(400));
        assert_eq!(s.procs[0].inaccessible, Some(2));
        assert_eq!(s.procs[0].truncated, Some(true));
        // A complete tick is explicit and distinguishable from unknown.
        assert_eq!(s.procs[1].inaccessible, Some(0));
        assert_eq!(s.procs[1].truncated, Some(false));
        // A legacy tick with no coverage metadata stays None (unknown), not
        // Some(0)/Some(false) which would claim complete observation.
        assert_eq!(s.procs[2].total_threads, None);
        assert_eq!(s.procs[2].inaccessible, None);
        assert_eq!(s.procs[2].truncated, None);
        // Downstream analysis can distinguish the three states.
        assert_eq!(s.process_coverage_incomplete(), Some(true));
        let complete = "{\"collector\":\"proc\",\"wall_ms\":1,\"mono_ms\":0,\
              \"total_threads\":400,\"inaccessible\":0,\"truncated\":false,\"top\":[]}\n";
        assert_eq!(
            load_session("c", complete)
                .unwrap()
                .process_coverage_incomplete(),
            Some(false)
        );
        let legacy = "{\"collector\":\"proc\",\"wall_ms\":1,\"mono_ms\":0,\"top\":[]}\n";
        assert_eq!(
            load_session("l", legacy)
                .unwrap()
                .process_coverage_incomplete(),
            None,
            "absent coverage metadata must read as unknown, not complete"
        );
    }
}
