//! OS power-state collector: active scheme + its policy settings.
//!
//! Everything here is readable without admin via powrprof/ntdll (verified
//! live): scheme GUID, CPU min/max state, EPP, display/sleep/hibernate
//! timeouts, brightness policy, battery alert levels, timer resolution.
//! Setting GUIDs were transcribed from `powercfg /query` on 2026-09-17 —
//! a wrong GUID surfaces as Unavailable, never as a wrong value.
//! Power requests, wake timers, and Energy Saver state need elevation or
//! WinRT and stay Unavailable with reasons.

use crate::collector::{Collector, CollectorError};
use pf_core::telemetry::{Clock, ClockStamp, FieldMeta, Provenance, Reading, escape_json};

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PowerGuid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

// --- Verified against `powercfg /query` (see module docs) ---
// Source: subgroup/setting names match powrprof.h POWER_SETTING_GUIDs where
// documented (e.g. GUID_PROCESSOR_SETTINGS_SUBGROUP == SUB_PROCESSOR,
// GUID_PROCESSOR_THROTTLE_MINIMUM == PROCTHROTTLEMIN,
// GUID_PROCESSOR_THROTTLE_MAXIMUM == PROCTHROTTLEMAX,
// GUID_PROCESSOR_PERF_ENERGY_PERF_PREFERENCE == PERFEPP,
// GUID_VIDEO_SUBGROUP == SUB_VIDEO, GUID_VIDEO_ANNOYANCE_TIMEOUT == VIDEOIDLE,
// GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS == VIDEONORMALLEVEL,
// GUID_SLEEP_SUBGROUP == SUB_SLEEP, GUID_STANDBY_TIMEOUT == STANDBYIDLE,
// GUID_HIBERNATE_TIMEOUT == HIBERNATEIDLE, GUID_BATTERY_SUBGROUP ==
// SUB_BATTERY; low/critical notification levels likewise). String values
// below are powercfg-verified on 2026-09-17 and are the single source for
// powercfg-map lookups: pick() resolves through guid_for(), so literals
// appear exactly once in this table, never duplicated at call sites.
//
// SAFETY: PowerGuid is a repr(C) 16-byte GUID POD mirroring powrprof.h.
// Layout is asserted at runtime in read() (degrades gracefully) and in
// tests; a mismatch never produces a wrong value, only Unavailable.
#[cfg(windows)]
const SUB_PROCESSOR: PowerGuid = PowerGuid {
    d1: 0x54533251,
    d2: 0x82be,
    d3: 0x4824,
    d4: [0x96, 0xc1, 0x47, 0xb6, 0x0b, 0x74, 0x0d, 0x00],
};
#[cfg(windows)]
const PROCTHROTTLEMIN: PowerGuid = PowerGuid {
    d1: 0x893dee8e,
    d2: 0x2bef,
    d3: 0x41e0,
    d4: [0x89, 0xc6, 0xb5, 0x5d, 0x09, 0x29, 0x96, 0x4c],
};
#[cfg(windows)]
const PROCTHROTTLEMAX: PowerGuid = PowerGuid {
    d1: 0xbc5038f7,
    d2: 0x23e0,
    d3: 0x4960,
    d4: [0x96, 0xda, 0x33, 0xab, 0xaf, 0x59, 0x35, 0xec],
};
/// Documented PERFEPP; absent on the test machine (AMD/High-perf exposes
/// only min/max) — degrades to Unavailable there.
#[cfg(windows)]
const PERFEPP: PowerGuid = PowerGuid {
    d1: 0x36687f9e,
    d2: 0xe3a5,
    d3: 0x4dbf,
    d4: [0xb1, 0xdc, 0x75, 0x34, 0xe3, 0xab, 0x68, 0xc8],
};
#[cfg(windows)]
const SUB_VIDEO: PowerGuid = PowerGuid {
    d1: 0x7516b95f,
    d2: 0xf776,
    d3: 0x4464,
    d4: [0x8c, 0x53, 0x06, 0x16, 0x7f, 0x40, 0xcc, 0x99],
};
#[cfg(windows)]
const VIDEOIDLE: PowerGuid = PowerGuid {
    d1: 0x3c0bc021,
    d2: 0xc8a8,
    d3: 0x4e07,
    d4: [0xa9, 0x73, 0x6b, 0x14, 0xcb, 0xcb, 0x2b, 0x7e],
};
#[cfg(windows)]
const VIDEONORMALLEVEL: PowerGuid = PowerGuid {
    d1: 0xaded5e82,
    d2: 0xb909,
    d3: 0x4619,
    d4: [0x99, 0x49, 0xf5, 0xd7, 0x1d, 0xac, 0x0b, 0xcb],
};
#[cfg(windows)]
const SUB_SLEEP: PowerGuid = PowerGuid {
    d1: 0x238c9fa8,
    d2: 0x0aad,
    d3: 0x41ed,
    d4: [0x83, 0xf4, 0x97, 0xbe, 0x24, 0x2c, 0x8f, 0x20],
};
#[cfg(windows)]
const STANDBYIDLE: PowerGuid = PowerGuid {
    d1: 0x29f6c1db,
    d2: 0x86da,
    d3: 0x48c5,
    d4: [0x9f, 0xdb, 0xf2, 0xb6, 0x7b, 0x1f, 0x44, 0xda],
};
#[cfg(windows)]
const HIBERNATEIDLE: PowerGuid = PowerGuid {
    d1: 0x9d7815a6,
    d2: 0x7ee4,
    d3: 0x497e,
    d4: [0x88, 0x88, 0x51, 0x5a, 0x05, 0xf0, 0x23, 0x64],
};
#[cfg(windows)]
const SUB_BATTERY: PowerGuid = PowerGuid {
    d1: 0xe73a048d,
    d2: 0xbf27,
    d3: 0x4f12,
    d4: [0x97, 0x31, 0x8b, 0x20, 0x76, 0xe8, 0x89, 0x1f],
};
#[cfg(windows)]
const BATTERY_LOW: PowerGuid = PowerGuid {
    d1: 0x8183ba9a,
    d2: 0xe910,
    d3: 0x48da,
    d4: [0x87, 0x69, 0x14, 0xae, 0x6d, 0xc1, 0x17, 0x0a],
};
#[cfg(windows)]
const BATTERY_CRIT: PowerGuid = PowerGuid {
    d1: 0x9a66d8d7,
    d2: 0x4ff7,
    d3: 0x4ef9,
    d4: [0xb5, 0xa2, 0x5a, 0x32, 0x6c, 0xa2, 0xa4, 0x69],
};

/// Central GUID table: (symbolic name, lowercase guid string). Single
/// source for powercfg effective-map lookups; see source note above.
pub const GUIDS: &[(&str, &str)] = &[
    ("SUB_PROCESSOR", "54533251-82be-4824-96c1-47b60b740d00"),
    ("PROCTHROTTLEMIN", "893dee8e-2bef-41e0-89c6-b55d0929964c"),
    ("PROCTHROTTLEMAX", "bc5038f7-23e0-4960-96da-33abaf5935ec"),
    ("PERFEPP", "36687f9e-e3a5-4dbf-b1dc-7534e3ab68c8"),
    ("SUB_VIDEO", "7516b95f-f776-4464-8c53-06167f40cc99"),
    ("VIDEOIDLE", "3c0bc021-c8a8-4e07-a973-6b14cbcb2b7e"),
    ("VIDEONORMALLEVEL", "aded5e82-b909-4619-9949-f5d71dac0bcb"),
    ("SUB_SLEEP", "238c9fa8-0aad-41ed-83f4-97be242c8f20"),
    ("STANDBYIDLE", "29f6c1db-86da-48c5-9fdb-f2b67b1f44da"),
    ("HIBERNATEIDLE", "9d7815a6-7ee4-497e-8888-515a05f02364"),
    ("SUB_BATTERY", "e73a048d-bf27-4f12-9731-8b2076e8891f"),
    ("BATTERY_LOW", "8183ba9a-e910-48da-8769-14ae6dc1170a"),
    ("BATTERY_CRIT", "9a66d8d7-4ff7-4ef9-b5a2-5a326ca2a469"),
];

/// Resolve a symbolic GUID name through the central table. Empty string on
/// unknown names (lookup miss degrades to Unavailable downstream, never a
/// wrong value).
pub fn guid_for(name: &str) -> &'static str {
    GUIDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, g)| *g)
        .unwrap_or("")
}

/// GUID shape check: 8-4-4-4-12 lowercase/uppercase hex. Pure and tested.
pub fn is_guid_shape(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    const LENS: [usize; 5] = [8, 4, 4, 4, 12];
    for (p, want) in parts.iter().zip(LENS) {
        if p.len() != want || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    true
}

#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryTimerResolution(
        min_resolution: *mut u32,
        max_resolution: *mut u32,
        current_resolution: *mut u32,
    ) -> i32;
}

#[cfg(windows)]
#[link(name = "powrprof")]
unsafe extern "system" {
    fn PowerGetActiveScheme(user_root_key: usize, active_policy_guid: *mut usize) -> u32;
    fn PowerReadACValueIndex(
        root_key: usize,
        scheme: *const PowerGuid,
        subgroup: *const PowerGuid,
        setting: *const PowerGuid,
        r#type: *mut u32,
        buffer: *mut u8,
        buffer_size: *mut u32,
    ) -> u32;
    fn PowerReadDCValueIndex(
        root_key: usize,
        scheme: *const PowerGuid,
        subgroup: *const PowerGuid,
        setting: *const PowerGuid,
        r#type: *mut u32,
        buffer: *mut u8,
        buffer_size: *mut u32,
    ) -> u32;
}

#[cfg(windows)]
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoTaskMemFree(pv: usize);
}

#[cfg(windows)]
const HKEY_CURRENT_USER: usize = 0x8000_0001;

#[derive(Debug, Clone)]
pub struct AcDc<T> {
    pub ac: T,
    pub dc: T,
}

#[derive(Debug, Clone)]
pub struct OsPowerRaw {
    pub scheme_guid: Reading<String>,
    pub scheme_name: String,
    pub timer_resolution_ms: Reading<f64>,
    /// Wall ms of the powercfg effective-value snapshot (0 = none taken).
    pub policy_snapshot_wall_ms: u64,
    pub cpu_min: AcDc<Reading<u32>>,
    pub cpu_max: AcDc<Reading<u32>>,
    pub epp: AcDc<Reading<u32>>,
    pub display_timeout_s: AcDc<Reading<u32>>,
    pub brightness_pct: AcDc<Reading<u32>>,
    pub sleep_timeout_s: AcDc<Reading<u32>>,
    pub hibernate_timeout_s: AcDc<Reading<u32>>,
    pub low_batt_pct: AcDc<Reading<u32>>,
    pub crit_batt_pct: AcDc<Reading<u32>>,
}

/// Unavailable placeholder for any field type (non-Windows builds).
fn na<T>() -> Reading<T> {
    Reading::Unavailable("Windows required")
}

impl Default for OsPowerRaw {
    fn default() -> Self {
        OsPowerRaw {
            scheme_guid: na(),
            scheme_name: "unknown".to_string(),
            timer_resolution_ms: na(),
            policy_snapshot_wall_ms: 0,
            cpu_min: AcDc { ac: na(), dc: na() },
            cpu_max: AcDc { ac: na(), dc: na() },
            epp: AcDc { ac: na(), dc: na() },
            display_timeout_s: AcDc { ac: na(), dc: na() },
            brightness_pct: AcDc { ac: na(), dc: na() },
            sleep_timeout_s: AcDc { ac: na(), dc: na() },
            hibernate_timeout_s: AcDc { ac: na(), dc: na() },
            low_batt_pct: AcDc { ac: na(), dc: na() },
            crit_batt_pct: AcDc { ac: na(), dc: na() },
        }
    }
}

/// Friendly names for the three inbox schemes; everything else is custom
/// (reported with its GUID, never mislabeled).
pub fn scheme_name(guid: &str) -> String {
    match guid.to_lowercase().as_str() {
        "381b4222-f694-41f0-9685-ff5bb260df2e" => "Balanced".to_string(),
        "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c" => "High performance".to_string(),
        "a1841308-3541-4fab-bc81-70f8c81f536" => "Power saver".to_string(),
        _ => format!("custom ({guid})"),
    }
}

/// Display-only timeout rendering: 0 means never.
pub fn format_timeout(secs: u32) -> String {
    if secs == 0 {
        return "never".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs.is_multiple_of(60) {
        return format!("{}m", secs / 60);
    }
    format!("{}m{}s", secs / 60, secs % 60)
}

#[cfg(windows)]
fn format_guid(g: &PowerGuid) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.d1, g.d2, g.d3, g.d4[0], g.d4[1], g.d4[2], g.d4[3], g.d4[4], g.d4[5], g.d4[6], g.d4[7]
    )
}

#[cfg(windows)]
fn active_scheme() -> Result<(PowerGuid, String), String> {
    let mut guid_ptr: usize = 0;
    // SAFETY: receives a CoTaskMem-allocated GUID, freed below on success.
    let rc = unsafe { PowerGetActiveScheme(HKEY_CURRENT_USER, &mut guid_ptr) };
    if rc != 0 || guid_ptr == 0 {
        return Err("PowerGetActiveScheme failed".to_string());
    }
    // SAFETY: 16-byte GUID written by the API.
    let guid = unsafe { *(guid_ptr as *const PowerGuid) };
    unsafe { CoTaskMemFree(guid_ptr) };
    let s = format_guid(&guid);
    Ok((guid, s))
}

#[cfg(windows)]
fn read_index(ac: bool, scheme: &PowerGuid, sub: &PowerGuid, set: &PowerGuid) -> Reading<u32> {
    // Root must match the store PowerGetActiveScheme read from; NULL lies
    // (success without writing) on some machines, so belt-and-braces: a
    // sentinel proves the buffer was actually written.
    const SENTINEL: u32 = 0xDEADBEEF;
    let mut ty: u32 = 0;
    let mut val: u32 = SENTINEL;
    let mut size: u32 = 4;
    // SAFETY: plain scalar out-params, return status checked.
    let rc = unsafe {
        if ac {
            PowerReadACValueIndex(
                HKEY_CURRENT_USER,
                scheme,
                sub,
                set,
                &mut ty,
                &mut val as *mut u32 as *mut u8,
                &mut size,
            )
        } else {
            PowerReadDCValueIndex(
                HKEY_CURRENT_USER,
                scheme,
                sub,
                set,
                &mut ty,
                &mut val as *mut u32 as *mut u8,
                &mut size,
            )
        }
    };
    if rc != 0 {
        return Reading::Unavailable("no stored override for this setting");
    }
    if val == SENTINEL {
        return Reading::Unavailable("API returned success without data");
    }
    Reading::Measured(val)
}

/// Parse `powercfg /query` into (subgroup, setting) -> (AC, DC) index map.
/// Only GUID-anchored lines are trusted; friendly names localize, GUIDs
/// and hex values do not.
pub fn parse_powercfg_query(text: &str) -> std::collections::HashMap<(String, String), (u32, u32)> {
    fn guid_of(line: &str) -> String {
        line.split_whitespace().next().unwrap_or("").to_lowercase()
    }
    fn hex_of(line: &str) -> Option<u32> {
        u32::from_str_radix(line.trim().trim_start_matches("0x").trim(), 16).ok()
    }
    let mut map = std::collections::HashMap::new();
    let mut sub = String::new();
    let mut set = String::new();
    let mut ac: Option<u32> = None;
    for raw in text.lines() {
        let t = raw.trim();
        if let Some(rest) = t.strip_prefix("Subgroup GUID:") {
            sub = guid_of(rest);
        } else if let Some(rest) = t.strip_prefix("Power Setting GUID:") {
            set = guid_of(rest);
            ac = None;
        } else if let Some(rest) = t.strip_prefix("Current AC Power Setting Index:") {
            ac = hex_of(rest);
        } else if let Some(rest) = t.strip_prefix("Current DC Power Setting Index:") {
            if let (Some(a), Some(d)) = (ac, hex_of(rest))
                && !sub.is_empty()
                && !set.is_empty()
            {
                map.insert((sub.clone(), set.clone()), (a, d));
            }
            ac = None;
        }
    }
    map
}

/// One `powercfg /query SCHEME_CURRENT` snapshot (~0.5s, so only at init
/// and on scheme change — never per tick).
#[cfg(windows)]
type EffectiveMap = std::collections::HashMap<(String, String), (u32, u32)>;

#[cfg(windows)]
fn query_effective() -> Result<EffectiveMap, String> {
    let out = std::process::Command::new("powercfg")
        .args(["/query", "SCHEME_CURRENT"])
        .output()
        .map_err(|e| format!("powercfg spawn failed: {e}"))?;
    if !out.status.success() {
        return Err("powercfg /query failed".to_string());
    }
    Ok(parse_powercfg_query(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(windows)]
fn read_timer_resolution() -> Reading<f64> {
    let mut min = 0u32;
    let mut max = 0u32;
    let mut cur = 0u32;
    // SAFETY: plain out-params, return status checked.
    let status = unsafe { NtQueryTimerResolution(&mut min, &mut max, &mut cur) };
    if status != 0 {
        return Reading::Unavailable("NtQueryTimerResolution failed");
    }
    Reading::Measured(cur as f64 / 10_000.0)
}

pub struct OsPowerCollector {
    effective: std::collections::HashMap<(String, String), (u32, u32)>,
    eff_scheme: String,
    eff_snapshot_ms: u64,
}

impl OsPowerCollector {
    pub fn new() -> Self {
        OsPowerCollector {
            effective: std::collections::HashMap::new(),
            eff_scheme: String::new(),
            eff_snapshot_ms: 0,
        }
    }

    /// One typed read; feeds both store and dashboard (collect once).
    /// Policy values resolve as: powercfg effective snapshot (refreshed at
    /// init and on scheme change) -> stored API override -> Unavailable.
    pub fn read(&mut self, stamp: ClockStamp) -> OsPowerRaw {
        #[cfg(windows)]
        {
            // Runtime ABI guard: PowerGuid must be 16 bytes (powrprof.h
            // GUID). On mismatch every policy field degrades to Unavailable
            // rather than calling the API with a corrupt layout.
            if std::mem::size_of::<PowerGuid>() != 16 {
                return OsPowerRaw {
                    scheme_guid: Reading::Unavailable("FFI layout mismatch: PowerGuid"),
                    timer_resolution_ms: read_timer_resolution(),
                    policy_snapshot_wall_ms: 0,
                    ..OsPowerRaw::default()
                };
            }
            let timer_resolution_ms = read_timer_resolution();
            let (scheme, guid_str) = match active_scheme() {
                Ok((g, s)) => (Some(g), Reading::Measured(s)),
                Err(_) => (
                    None,
                    Reading::<String>::Unavailable("PowerGetActiveScheme failed"),
                ),
            };
            let pair = |sub: &PowerGuid, set: &PowerGuid| -> AcDc<Reading<u32>> {
                match &scheme {
                    Some(s) => AcDc {
                        ac: read_index(true, s, sub, set),
                        dc: read_index(false, s, sub, set),
                    },
                    None => AcDc {
                        ac: Reading::Unavailable("active scheme unknown"),
                        dc: Reading::Unavailable("active scheme unknown"),
                    },
                }
            };
            // Refresh the effective-value snapshot at init and on scheme
            // change (powercfg spawn ~0.5s; never per tick).
            let guid_now = guid_str.value().cloned().unwrap_or_default();
            if (self.effective.is_empty() || self.eff_scheme != guid_now)
                && let Ok(map) = query_effective()
            {
                self.effective = map;
                self.eff_scheme = guid_now.clone();
                self.eff_snapshot_ms = stamp.wall_millis;
            }
            // Effective snapshot wins; stored override is the fallback.
            // pick() resolves symbolic names through the central GUIDS
            // table, so guid literals appear once (in GUIDS), never here.
            let pick =
                |sub_name: &str, set_name: &str, api: AcDc<Reading<u32>>| -> AcDc<Reading<u32>> {
                    let key = (
                        guid_for(sub_name).to_string(),
                        guid_for(set_name).to_string(),
                    );
                    match self.effective.get(&key) {
                        Some((a, d)) => AcDc {
                            ac: Reading::Measured(*a),
                            dc: Reading::Measured(*d),
                        },
                        None => api,
                    }
                };
            OsPowerRaw {
                scheme_name: match &guid_str {
                    Reading::Measured(s) => scheme_name(s),
                    _ => "unknown".to_string(),
                },
                scheme_guid: guid_str,
                timer_resolution_ms,
                policy_snapshot_wall_ms: self.eff_snapshot_ms,
                cpu_min: pick(
                    "SUB_PROCESSOR",
                    "PROCTHROTTLEMIN",
                    pair(&SUB_PROCESSOR, &PROCTHROTTLEMIN),
                ),
                cpu_max: pick(
                    "SUB_PROCESSOR",
                    "PROCTHROTTLEMAX",
                    pair(&SUB_PROCESSOR, &PROCTHROTTLEMAX),
                ),
                epp: pick("SUB_PROCESSOR", "PERFEPP", pair(&SUB_PROCESSOR, &PERFEPP)),
                display_timeout_s: pick("SUB_VIDEO", "VIDEOIDLE", pair(&SUB_VIDEO, &VIDEOIDLE)),
                brightness_pct: pick(
                    "SUB_VIDEO",
                    "VIDEONORMALLEVEL",
                    pair(&SUB_VIDEO, &VIDEONORMALLEVEL),
                ),
                sleep_timeout_s: pick("SUB_SLEEP", "STANDBYIDLE", pair(&SUB_SLEEP, &STANDBYIDLE)),
                hibernate_timeout_s: pick(
                    "SUB_SLEEP",
                    "HIBERNATEIDLE",
                    pair(&SUB_SLEEP, &HIBERNATEIDLE),
                ),
                low_batt_pct: pick(
                    "SUB_BATTERY",
                    "BATTERY_LOW",
                    pair(&SUB_BATTERY, &BATTERY_LOW),
                ),
                crit_batt_pct: pick(
                    "SUB_BATTERY",
                    "BATTERY_CRIT",
                    pair(&SUB_BATTERY, &BATTERY_CRIT),
                ),
            }
        }
        #[cfg(not(windows))]
        {
            OsPowerRaw::default()
        }
    }

    /// Render a previously-read snapshot (collect once, reuse).
    pub fn format_json(
        raw: &OsPowerRaw,
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        use pf_core::telemetry::{Provenance, UnavailKind, json_num, json_str};
        fn num(r: &Reading<u32>) -> String {
            // Policy absence means no stored override (Unsupported); the
            // reason text carries the detail.
            json_num(
                r.value().map(|v| *v as f64),
                r.provenance(),
                UnavailKind::Unsupported,
                r.reason().unwrap_or("no stored override"),
            )
        }
        fn pair(p: &AcDc<Reading<u32>>) -> String {
            format!("{{\"ac\":{},\"dc\":{}}}", num(&p.ac), num(&p.dc))
        }
        let scheme = match &raw.scheme_guid {
            Reading::Measured(s) | Reading::Derived(s) | Reading::Estimated(s) => json_str(
                Some(s),
                raw.scheme_guid.provenance(),
                UnavailKind::TransientError,
                "PowerGetActiveScheme failed",
            ),
            Reading::Unavailable(r) => json_str(
                None,
                Provenance::Unavailable,
                UnavailKind::TransientError,
                r,
            ),
        };
        let timer = match &raw.timer_resolution_ms {
            Reading::Measured(v) | Reading::Derived(v) | Reading::Estimated(v) => json_num(
                Some(*v),
                raw.timer_resolution_ms.provenance(),
                UnavailKind::TransientError,
                "NtQueryTimerResolution failed",
            ),
            Reading::Unavailable(r) => json_num(
                None,
                Provenance::Unavailable,
                UnavailKind::TransientError,
                r,
            ),
        };
        format!(
            "{{\"collector\":\"os_power\",\"wall_ms\":{},\"mono_ms\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{},\
             \"active_scheme\":{scheme},\"scheme_name\":\"{}\",\"timer_resolution_ms\":{timer},\
             \"policy_snapshot_wall_ms\":{},\
             \"cpu_min_pct\":{},\"cpu_max_pct\":{},\"epp\":{},\
             \"display_timeout_s\":{},\"brightness_pct\":{},\
             \"sleep_timeout_s\":{},\"hibernate_timeout_s\":{},\
             \"low_batt_pct\":{},\"crit_batt_pct\":{}}}",
            stamp.wall_millis,
            stamp.mono_millis,
            t_start_ms,
            t_end_ms,
            escape_json(&raw.scheme_name),
            raw.policy_snapshot_wall_ms,
            pair(&raw.cpu_min),
            pair(&raw.cpu_max),
            pair(&raw.epp),
            pair(&raw.display_timeout_s),
            pair(&raw.brightness_pct),
            pair(&raw.sleep_timeout_s),
            pair(&raw.hibernate_timeout_s),
            pair(&raw.low_batt_pct),
            pair(&raw.crit_batt_pct),
        )
    }
}

impl Default for OsPowerCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for OsPowerCollector {
    fn name(&self) -> &'static str {
        "os_power"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                name: "os.active_power_scheme",
                unit: "guid+name",
                source: "PowerGetActiveScheme",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "Name mapped for inbox schemes; custom schemes keep GUID; 30 s steady state (monitor samples 5 s for the first two ticks as a fast initial snapshot)",
            },
            FieldMeta {
                name: "os.cpu_min_max_pct",
                unit: "% ac/dc",
                source: "PowerReadAC/DCValueIndex SUB_PROCESSOR PROCTHROTTLEMIN/MAX",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "DC values govern battery behavior; 5% min on DC here",
            },
            FieldMeta {
                name: "os.epp",
                unit: "0-100 ac/dc",
                source: "PowerReadAC/DCValueIndex SUB_PROCESSOR PERFEPP",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "Absent on this machine (AMD/High-perf); 0 favors performance",
            },
            FieldMeta {
                name: "os.display_timeout_s",
                unit: "s ac/dc",
                source: "PowerReadAC/DCValueIndex SUB_VIDEO VIDEOIDLE",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "0 = never",
            },
            FieldMeta {
                name: "os.brightness_policy_pct",
                unit: "% ac/dc",
                source: "PowerReadAC/DCValueIndex SUB_VIDEO VIDEONORMALLEVEL",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "Policy, not sensor; cross-checked vs live brightness at implementation time",
            },
            FieldMeta {
                name: "os.sleep_timeout_s",
                unit: "s ac/dc",
                source: "PowerReadAC/DCValueIndex SUB_SLEEP STANDBYIDLE",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "0 = never",
            },
            FieldMeta {
                name: "os.hibernate_timeout_s",
                unit: "s ac/dc",
                source: "PowerReadAC/DCValueIndex SUB_SLEEP HIBERNATEIDLE",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "0 = never",
            },
            FieldMeta {
                name: "os.battery_levels_pct",
                unit: "% ac/dc",
                source: "PowerReadAC/DCValueIndex SUB_BATTERY low/critical",
                provenance: Provenance::Measured,
                interval_ms: 60_000,
                requires_admin: false,
                notes: "Thresholds behind critical-action forensics",
            },
            FieldMeta {
                name: "os.timer_resolution_ms",
                unit: "ms",
                source: "NtQueryTimerResolution",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "1ms-class values implicate multimedia timers / wakeups",
            },
            FieldMeta {
                name: "os.power_requests",
                unit: "events",
                source: "powercfg /requests (planned, privileged)",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: true,
                notes: "Requires elevation; needs privileged helper or ETW",
            },
            FieldMeta {
                name: "os.energy_saver",
                unit: "state",
                source: "WinRT PowerManager (planned)",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "No Win32 query API; no scheme subgroup for it",
            },
            FieldMeta {
                name: "os.session_events",
                unit: "events",
                source: "ETW / session notifications (planned)",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "Lock/unlock, sleep transitions for timeline correlation",
            },
        ]
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let t0 = clock.stamp();
        let raw = self.read(t0);
        let t1 = clock.stamp();
        Ok(Self::format_json(&raw, t1, t0.mono_millis, t1.mono_millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powercfg_parser_reads_effective_values() {
        let text = "\
Power Scheme GUID: 8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c  (High performance)\n\
  Subgroup GUID: 54533251-82be-4824-96c1-47b60b740d00  (Processor power management)\n\
    Power Setting GUID: 893dee8e-2bef-41e0-89c6-b55d0929964c  (Minimum processor state)\n\
    Current AC Power Setting Index: 0x00000064\n\
    Current DC Power Setting Index: 0x00000005\n\
    Power Setting GUID: bc5038f7-23e0-4960-96da-33abaf5935ec  (Maximum processor state)\n\
    Current AC Power Setting Index: 0x00000064\n\
    Current DC Power Setting Index: 0x00000064\n\
  Subgroup GUID: 7516B95F-F776-4464-8C53-06167F40CC99  (Display)\n\
    Power Setting GUID: ADED5E82-B909-4619-9949-F5D71DAC0BCB  (Display brightness)\n\
    Current AC Power Setting Index: 0x00000028\n\
    Current DC Power Setting Index: 0x00000028\n";
        let m = parse_powercfg_query(text);
        assert_eq!(
            m[&(
                "54533251-82be-4824-96c1-47b60b740d00".to_string(),
                "893dee8e-2bef-41e0-89c6-b55d0929964c".to_string()
            )],
            (100, 5)
        );
        assert_eq!(
            m[&(
                "54533251-82be-4824-96c1-47b60b740d00".to_string(),
                "bc5038f7-23e0-4960-96da-33abaf5935ec".to_string()
            )],
            (100, 100)
        );
        // GUID case is normalized.
        assert_eq!(
            m[&(
                "7516b95f-f776-4464-8c53-06167f40cc99".to_string(),
                "aded5e82-b909-4619-9949-f5d71dac0bcb".to_string()
            )],
            (40, 40)
        );
        // Garbage and incomplete pairs are ignored, never fabricated.
        assert!(
            parse_powercfg_query("nothing here\nCurrent AC Power Setting Index: 0x1\n").is_empty()
        );
        assert!(parse_powercfg_query(
            "  Subgroup GUID: aaaa\n    Power Setting GUID: bbbb\n    Current AC Power Setting Index: 0x1\n"
        )
        .is_empty());
    }

    #[test]
    fn scheme_names() {
        assert_eq!(
            scheme_name("381b4222-f694-41f0-9685-ff5bb260df2e"),
            "Balanced"
        );
        assert_eq!(
            scheme_name("8C5E7FDA-E8BF-4A96-9A85-A6E23A8C635C"),
            "High performance"
        );
        assert_eq!(
            scheme_name("a1841308-3541-4fab-bc81-70f8c81f536"),
            "Power saver"
        );
        assert!(scheme_name("12345678-1234-1234-1234-123456789abc").starts_with("custom"));
    }

    #[test]
    fn timeout_rendering() {
        assert_eq!(format_timeout(0), "never");
        assert_eq!(format_timeout(30), "30s");
        assert_eq!(format_timeout(600), "10m");
        assert_eq!(format_timeout(90), "1m30s");
    }

    #[test]
    fn guid_shape_checks() {
        assert!(is_guid_shape("54533251-82be-4824-96c1-47b60b740d00"));
        assert!(is_guid_shape("ADED5E82-B909-4619-9949-F5D71DAC0BCB"));
        assert!(!is_guid_shape("not-a-guid"));
        assert!(!is_guid_shape("54533251-82be-4824-96c1-47b60b740d0"));
        assert!(!is_guid_shape("54533251-82be-4824-96c1-47b60b740d0g"));
        assert!(!is_guid_shape(""));
    }

    #[test]
    fn guid_table_covers_every_pick() {
        // Every (subgroup, setting) pair used by pick() in read() must
        // resolve through the central table and have valid GUID shape.
        let pairs = [
            ("SUB_PROCESSOR", "PROCTHROTTLEMIN"),
            ("SUB_PROCESSOR", "PROCTHROTTLEMAX"),
            ("SUB_PROCESSOR", "PERFEPP"),
            ("SUB_VIDEO", "VIDEOIDLE"),
            ("SUB_VIDEO", "VIDEONORMALLEVEL"),
            ("SUB_SLEEP", "STANDBYIDLE"),
            ("SUB_SLEEP", "HIBERNATEIDLE"),
            ("SUB_BATTERY", "BATTERY_LOW"),
            ("SUB_BATTERY", "BATTERY_CRIT"),
        ];
        for (sub, set) in pairs {
            let sg = guid_for(sub);
            let st = guid_for(set);
            assert!(!sg.is_empty(), "missing table entry for {sub}");
            assert!(!st.is_empty(), "missing table entry for {set}");
            assert!(is_guid_shape(sg), "bad GUID shape for {sub}: {sg}");
            assert!(is_guid_shape(st), "bad GUID shape for {set}: {st}");
        }
        // Whole table parses as GUID shape; names are unique.
        let mut names = std::collections::HashSet::new();
        for (name, guid) in GUIDS {
            assert!(names.insert(*name), "duplicate GUIDS entry for {name}");
            assert!(is_guid_shape(guid), "bad GUID shape for {name}: {guid}");
        }
        assert_eq!(GUIDS.len(), 13);
        assert_eq!(guid_for("NO_SUCH_SETTING"), "");
    }

    #[cfg(windows)]
    #[test]
    fn power_guid_layout_is_16_bytes() {
        assert_eq!(std::mem::size_of::<PowerGuid>(), 16);
    }
}
