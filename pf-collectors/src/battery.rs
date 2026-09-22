//! Battery collector: ground-truth system power when unplugged.
//!
//! Sources (both work without admin, verified on test machine):
//!   * `GetSystemPowerStatus` (kernel32) — charge %, AC state. Cached/coarse.
//!   * `CallNtPowerInformation(SystemBatteryState)` (powrprof) — capacities,
//!     voltage-class rate in mW, remaining/full-charge capacity in mWh.
//!
//! Design capacity and temperature are NOT exposed by these paths and are
//! reported as Unavailable until the IOCTL battery-query path lands.

use crate::batinfo;
use crate::collector::{Collector, CollectorError};
use pf_core::stats;
use pf_core::telemetry::{
    Clock, ClockStamp, FieldMeta, Provenance, Reading, UnavailKind, escape_json, quality_for_kind,
};

#[cfg(windows)]
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
struct SystemPowerStatus {
    ac_line_status: u8,
    battery_flag: u8,
    battery_life_percent: u8,
    system_status_flag: u8,
    battery_life_time: u32,
    battery_full_life_time: u32,
}

/// SYSTEM_BATTERY_STATE as returned by CallNtPowerInformation(level 5).
#[cfg(windows)]
#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
struct SystemBatteryState {
    ac_on: u8,
    battery_present: u8,
    charging: u8,
    discharging: u8,
    spare: [u8; 4],
    max_capacity_mwh: u32,
    remaining_capacity_mwh: u32,
    rate_mw: i32,
    estimated_time_secs: u32,
    default_alert1: u32,
    default_alert2: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
}

#[cfg(windows)]
#[link(name = "powrprof")]
unsafe extern "system" {
    fn CallNtPowerInformation(
        information_level: i32,
        input_buffer: *const std::ffi::c_void,
        input_length: u32,
        output_buffer: *mut std::ffi::c_void,
        output_length: u32,
    ) -> i32;
}

#[cfg(windows)]
const SYSTEM_BATTERY_STATE_LEVEL: i32 = 5;

fn charge_percent(value: u8, flags: u8) -> Reading<u8> {
    if flags == 255 {
        Reading::Unavailable("battery status unknown")
    } else if flags & 128 != 0 {
        Reading::Unavailable("no system battery")
    } else if value <= 100 {
        Reading::Measured(value)
    } else {
        Reading::Unavailable("charge not reported")
    }
}

/// Snapshot of raw battery state with provenance attached.
#[derive(Debug, Clone)]
pub struct BatteryRaw {
    pub ac_connected: Reading<bool>,
    pub charge_percent: Reading<u8>,
    pub present: bool,
    pub max_capacity_mwh: Reading<u32>,
    pub remaining_capacity_mwh: Reading<u32>,
    /// Signed rate in mW: positive = charging, negative = discharging.
    /// Sign convention verified empirically (positive while charging).
    pub rate_mw: Reading<i32>,
}

impl BatteryRaw {
    /// Instantaneous discharge power in W. Unavailable when not discharging
    /// (never report charge power as discharge). The mW->W scale is a unit
    /// conversion, so the result is DERIVED even when rate_mw was measured;
    /// a genuine measured value of 0 W (idle) stays a real derived zero.
    pub fn discharge_w(&self) -> Reading<f64> {
        match &self.rate_mw {
            Reading::Measured(r) | Reading::Derived(r) if *r < 0 => {
                Reading::Derived(-(*r) as f64 / 1000.0)
            }
            Reading::Measured(_) | Reading::Derived(_) => {
                Reading::Unavailable("not discharging (on AC or idle)")
            }
            Reading::Estimated(_) => Reading::Unavailable("rate is only estimated"),
            Reading::Unavailable(r) => Reading::Unavailable(r),
        }
    }

    /// Instantaneous charge power in W. Unavailable unless charging; the
    /// mW->W conversion makes the result Derived (see discharge_w).
    pub fn charge_w(&self) -> Reading<f64> {
        match &self.rate_mw {
            Reading::Measured(r) | Reading::Derived(r) if *r > 0 => {
                Reading::Derived(*r as f64 / 1000.0)
            }
            Reading::Measured(_) | Reading::Derived(_) => Reading::Unavailable("not charging"),
            Reading::Estimated(_) => Reading::Unavailable("rate is only estimated"),
            Reading::Unavailable(r) => Reading::Unavailable(r),
        }
    }

    pub fn remaining_wh(&self) -> Reading<f64> {
        match &self.remaining_capacity_mwh {
            Reading::Measured(v) | Reading::Derived(v) => Reading::Derived(*v as f64 / 1000.0),
            Reading::Estimated(_) => Reading::Unavailable("capacity is only estimated"),
            Reading::Unavailable(r) => Reading::Unavailable(r),
        }
    }
}

#[cfg(windows)]
fn read_windows() -> Result<BatteryRaw, String> {
    let mut sps = SystemPowerStatus::default();
    // SAFETY: fixed-size repr(C) struct, checked return value.
    let ok = unsafe { GetSystemPowerStatus(&mut sps) };
    if ok == 0 {
        return Err(format!(
            "GetSystemPowerStatus failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut state = SystemBatteryState::default();
    // SAFETY: output buffer sized exactly to the struct.
    let nt = unsafe {
        CallNtPowerInformation(
            SYSTEM_BATTERY_STATE_LEVEL,
            std::ptr::null(),
            0,
            &mut state as *mut SystemBatteryState as *mut std::ffi::c_void,
            std::mem::size_of::<SystemBatteryState>() as u32,
        )
    };
    if nt != 0 {
        return Err(format!("CallNtPowerInformation failed: NTSTATUS {nt:#X}"));
    }
    let ac_connected = match sps.ac_line_status {
        0 => Reading::Measured(false),
        1 => Reading::Measured(true),
        _ => Reading::Unavailable("AC state unknown"),
    };
    let present = state.battery_present != 0;
    Ok(BatteryRaw {
        ac_connected,
        charge_percent: charge_percent(sps.battery_life_percent, sps.battery_flag),
        present,
        max_capacity_mwh: if present && state.max_capacity_mwh > 0 {
            Reading::Measured(state.max_capacity_mwh)
        } else {
            Reading::Unavailable("full-charge capacity not reported")
        },
        remaining_capacity_mwh: if present {
            Reading::Measured(state.remaining_capacity_mwh)
        } else {
            Reading::Unavailable("no system battery")
        },
        rate_mw: if present {
            Reading::Measured(state.rate_mw)
        } else {
            Reading::Unavailable("no system battery")
        },
    })
}

pub use pf_core::analysis::{HeadlineHealth, headline_health, per_battery_health};

pub struct BatteryCollector;

impl BatteryCollector {
    pub fn new() -> Self {
        BatteryCollector
    }

    pub fn read(&self) -> Result<BatteryRaw, String> {
        #[cfg(windows)]
        return read_windows();
        #[cfg(not(windows))]
        return Err("Windows required".to_string());
    }
}

impl Default for BatteryCollector {
    fn default() -> Self {
        Self::new()
    }
}

fn reading_num<T: std::fmt::Display>(r: &Reading<T>, kind: UnavailKind) -> String {
    match r {
        Reading::Measured(v) | Reading::Derived(v) | Reading::Estimated(v) => {
            format!(
                "{{\"v\":{v},\"p\":\"{}\",\"q\":0}}",
                r.provenance().as_str()
            )
        }
        Reading::Unavailable(reason) => {
            format!(
                "{{\"v\":null,\"p\":\"unavailable\",\"k\":{},\"q\":{},\"reason\":\"{}\"}}",
                kind as u8,
                quality_for_kind(kind) as u8,
                escape_json(reason)
            )
        }
    }
}

impl Collector for BatteryCollector {
    fn name(&self) -> &'static str {
        "battery"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                name: "battery.index_count",
                unit: "index/n",
                source: "cfgmgr32 battery interface enumeration",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Which physical battery is sampled; only index 0 is read (documented multi-battery limit)",
            },
            FieldMeta {
                name: "battery.charge_percent",
                unit: "%",
                source: "GetSystemPowerStatus",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Coarse, sometimes cached by Windows",
            },
            FieldMeta {
                name: "battery.ac_connected",
                unit: "bool",
                source: "GetSystemPowerStatus",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "255 = unknown",
            },
            FieldMeta {
                name: "battery.rate_mw",
                unit: "mW",
                source: "CallNtPowerInformation(SystemBatteryState)",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Signed: positive = charging, negative = discharging; discharge_w/charge_w are the mW->W conversion, hence Derived",
            },
            FieldMeta {
                name: "battery.remaining_mwh",
                unit: "mWh",
                source: "CallNtPowerInformation(SystemBatteryState)",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "",
            },
            FieldMeta {
                name: "battery.full_charge_mwh",
                unit: "mWh",
                source: "CallNtPowerInformation(SystemBatteryState)",
                provenance: Provenance::Measured,
                interval_ms: 60_000,
                requires_admin: false,
                notes: "Changes slowly; poll at low rate",
            },
            FieldMeta {
                name: "battery.design_mwh",
                unit: "mWh",
                source: "IOCTL_BATTERY_QUERY_INFORMATION(BatteryInformation)",
                provenance: Provenance::Measured,
                interval_ms: 60_000,
                requires_admin: false,
                notes: "Absent without an ACPI battery interface; never substitute full-charge",
            },
            FieldMeta {
                name: "battery.chemistry",
                unit: "tag",
                source: "IOCTL_BATTERY_QUERY_INFORMATION(BatteryInformation)",
                provenance: Provenance::Measured,
                interval_ms: 60_000,
                requires_admin: false,
                notes: "4-char code, e.g. LION",
            },
            FieldMeta {
                name: "battery.technology",
                unit: "raw",
                source: "IOCTL_BATTERY_QUERY_INFORMATION(BatteryInformation)",
                provenance: Provenance::Measured,
                interval_ms: 60_000,
                requires_admin: false,
                notes: "Numeric code as reported; vendor decoding pending",
            },
            FieldMeta {
                name: "battery.cycle_count",
                unit: "cycles",
                source: "IOCTL_BATTERY_QUERY_INFORMATION (cross-check: WMI BatteryCycleCount)",
                provenance: Provenance::Measured,
                interval_ms: 60_000,
                requires_admin: false,
                notes: "",
            },
            FieldMeta {
                name: "battery.health",
                unit: "ratio",
                source: "full_charge_mwh / design_mwh",
                provenance: Provenance::Derived,
                interval_ms: 60_000,
                requires_admin: false,
                notes: "Unavailable until design capacity is known",
            },
            FieldMeta {
                name: "battery.temperature_raw",
                unit: "raw u32",
                source: "IOCTL_BATTERY_QUERY_INFORMATION(BatteryTemperature)",
                provenance: Provenance::Measured,
                interval_ms: 5_000,
                requires_admin: false,
                notes: "Units suspected 0.1 K but UNVERIFIED — kept raw, never converted blindly",
            },
            FieldMeta {
                name: "battery.identity",
                unit: "strings",
                source: "IOCTL device-name/mfr-name/unique-id/mfg-date",
                provenance: Provenance::Measured,
                interval_ms: 60_000,
                requires_admin: false,
                notes: "For multi-battery disambiguation and aging analysis",
            },
        ]
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let t0 = clock.stamp();
        let raw = self.read().map_err(|e| CollectorError::new("battery", e))?;
        // Static IOCTL data for every physical battery (µs-scale), degrades
        // to Unavailable per-field when the interface is absent.
        let statics = batinfo::query_all();
        let t1 = clock.stamp();
        Ok(Self::format_json(
            &raw,
            &statics,
            t1,
            t0.mono_millis,
            t1.mono_millis,
        ))
    }
}

impl BatteryCollector {
    /// Render a previously-read snapshot. Separated from `read()` so
    /// callers can collect once and reuse the data (dashboard + store).
    /// `t_start_ms`/`t_end_ms` bound the actual acquisition work.
    ///
    /// `statics` holds every physical battery interface; the aggregate
    /// Windows state (`raw`) is never merged with a per-battery capacity.
    pub fn format_json(
        raw: &BatteryRaw,
        statics: &[(usize, batinfo::BatteryStatic)],
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        let primary = statics.first().map(|(_, s)| s);
        // Health is per physical battery: both capacities come from the same
        // interface. The aggregate Windows full-charge figure is not used.
        let health: Reading<f64> = match primary.and_then(|s| s.designed_mwh.zip(s.full_mwh)) {
            Some((design, full)) => match stats::health_ratio(full as f64, design as f64) {
                Some(h) => Reading::Derived(h),
                None => Reading::Unavailable("invalid capacity values"),
            },
            None => Reading::Unavailable("design capacity unknown"),
        };
        let mfg_date = primary
            .and_then(|s| s.mfg_date)
            .map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}"));
        let bat_count = if statics.is_empty() {
            batinfo::count_interfaces()
        } else {
            statics.len()
        };
        let bat_index = statics.first().map(|(i, _)| *i as u32);
        let mut batteries = String::from("[");
        for (n, (index, s)) in statics.iter().enumerate() {
            if n > 0 {
                batteries.push(',');
            }
            batteries.push_str(&format!(
                "{{\"index\":{index},\"designed_mwh\":{},\"full_mwh\":{},\"cycle_count\":{},\
                 \"chemistry\":{},\"device_name\":{},\"unique_id\":{}}}",
                opt_u32(s.designed_mwh),
                opt_u32(s.full_mwh),
                opt_u32(s.cycle_count),
                opt_str(&s.chemistry),
                opt_str(&s.device_name),
                opt_str(&s.unique_id),
            ));
        }
        batteries.push(']');
        format!(
            "{{\"collector\":\"battery\",\"wall_ms\":{},\"mono_ms\":{},\
             \"battery_index\":{},\"battery_count\":{},\"batteries\":{batteries},\
             \"ac\":{},\"charge_pct\":{},\"present\":{},\
             \"rate_mw\":{},\"remaining_mwh\":{},\"full_charge_mwh\":{},\
             \"discharge_w\":{},\"charge_w\":{},\
             \"design_mwh\":{},\"health\":{},\
             \"chemistry\":{},\"technology\":{},\"capabilities\":{},\
             \"cycle_count\":{},\"temperature_raw\":{},\
             \"device_name\":{},\"mfg_name\":{},\"unique_id\":{},\"mfg_date\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{}}}",
            stamp.wall_millis,
            stamp.mono_millis,
            opt_u32(bat_index),
            opt_u32((bat_count > 0).then_some(bat_count as u32)),
            reading_num(&raw.ac_connected, UnavailKind::TransientError),
            reading_num(&raw.charge_percent, UnavailKind::Unsupported),
            raw.present,
            reading_num(&raw.rate_mw, UnavailKind::Unsupported),
            reading_num(&raw.remaining_capacity_mwh, UnavailKind::Unsupported),
            reading_num(&raw.max_capacity_mwh, UnavailKind::Unsupported),
            reading_num(&raw.discharge_w(), UnavailKind::NotSampled),
            reading_num(&raw.charge_w(), UnavailKind::NotSampled),
            opt_u32(primary.and_then(|s| s.designed_mwh)),
            reading_num(&health, UnavailKind::Unsupported),
            opt_str(&primary.and_then(|s| s.chemistry.clone())),
            opt_u32(primary.and_then(|s| s.technology.map(|t| t as u32))),
            opt_hex(primary.and_then(|s| s.capabilities)),
            opt_u32(primary.and_then(|s| s.cycle_count)),
            opt_u32(primary.and_then(|s| s.temperature_raw)),
            opt_str(&primary.and_then(|s| s.device_name.clone())),
            opt_str(&primary.and_then(|s| s.mfg_name.clone())),
            opt_str(&primary.and_then(|s| s.unique_id.clone())),
            opt_str(&mfg_date),
            t_start_ms,
            t_end_ms,
        )
    }
}

/// Optional IOCTL fields: absent means the interface does not report them
/// (Unsupported); the reason string is preserved verbatim.
fn opt_u32(v: Option<u32>) -> String {
    pf_core::telemetry::json_num(
        v.map(|x| x as f64),
        Provenance::Measured,
        UnavailKind::Unsupported,
        "not reported by battery interface",
    )
}

fn opt_hex(v: Option<u32>) -> String {
    let s = v.map(|x| format!("{x:#X}"));
    pf_core::telemetry::json_str(
        s.as_deref(),
        Provenance::Measured,
        UnavailKind::Unsupported,
        "not reported by battery interface",
    )
}

fn opt_str(v: &Option<String>) -> String {
    pf_core::telemetry::json_str(
        v.as_deref(),
        Provenance::Measured,
        UnavailKind::Unsupported,
        "not reported by battery interface",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_charge() {
        assert_eq!(charge_percent(76, 0), Reading::Measured(76));
        assert_eq!(charge_percent(0, 0), Reading::Measured(0));
        assert_eq!(charge_percent(100, 8), Reading::Measured(100));
    }

    #[test]
    fn unavailable_is_not_zero() {
        assert!(matches!(charge_percent(255, 0), Reading::Unavailable(_)));
        assert!(matches!(charge_percent(101, 0), Reading::Unavailable(_)));
        assert!(matches!(charge_percent(50, 128), Reading::Unavailable(_)));
        assert!(matches!(charge_percent(50, 255), Reading::Unavailable(_)));
    }

    #[test]
    fn discharge_never_reports_charge_power() {
        let charging = BatteryRaw {
            ac_connected: Reading::Measured(true),
            charge_percent: Reading::Measured(52),
            present: true,
            max_capacity_mwh: Reading::Measured(37090),
            remaining_capacity_mwh: Reading::Measured(19848),
            rate_mw: Reading::Measured(15405),
        };
        assert!(matches!(charging.discharge_w(), Reading::Unavailable(_)));
        assert_eq!(charging.charge_w(), Reading::Derived(15.405));

        let discharging = BatteryRaw {
            rate_mw: Reading::Measured(-6400),
            ..charging
        };
        assert_eq!(discharging.discharge_w(), Reading::Derived(6.4));
        assert!(matches!(discharging.charge_w(), Reading::Unavailable(_)));
    }

    fn raw_with_rate(rate_mw: Reading<i32>) -> BatteryRaw {
        BatteryRaw {
            ac_connected: Reading::Measured(false),
            charge_percent: Reading::Measured(50),
            present: true,
            max_capacity_mwh: Reading::Measured(37090),
            remaining_capacity_mwh: Reading::Measured(19848),
            rate_mw,
        }
    }

    #[test]
    fn power_conversion_is_derived_and_keeps_real_zero() {
        // mW -> W is a unit conversion: the wire value was measured, the W
        // figure is derived, never labelled Measured.
        let charging = raw_with_rate(Reading::Measured(5000));
        assert_eq!(charging.charge_w(), Reading::Derived(5.0));
        assert_eq!(charging.charge_w().provenance(), Provenance::Derived);

        let discharging = raw_with_rate(Reading::Measured(-5000));
        assert_eq!(discharging.discharge_w(), Reading::Derived(5.0));
        assert_eq!(discharging.discharge_w().provenance(), Provenance::Derived);

        // A genuine zero rate is a real measured idle state: neither power
        // direction is claimed (0 mW is not discharging and not charging).
        let idle = raw_with_rate(Reading::Measured(0));
        assert!(matches!(idle.discharge_w(), Reading::Unavailable(_)));
        assert!(matches!(idle.charge_w(), Reading::Unavailable(_)));
        // remaining_wh conversion stays Derived and preserves a real zero.
        let rem = BatteryRaw {
            remaining_capacity_mwh: Reading::Measured(0),
            ..raw_with_rate(Reading::Measured(0))
        };
        assert_eq!(rem.remaining_wh(), Reading::Derived(0.0));
    }

    #[test]
    fn unavailable_rate_propagates_with_reason() {
        let raw = BatteryRaw {
            ac_connected: Reading::Unavailable("AC state unknown"),
            charge_percent: Reading::Unavailable("no system battery"),
            present: false,
            max_capacity_mwh: Reading::Unavailable("no system battery"),
            remaining_capacity_mwh: Reading::Unavailable("no system battery"),
            rate_mw: Reading::Unavailable("no system battery"),
        };
        assert!(matches!(raw.discharge_w(), Reading::Unavailable(_)));
        assert!(raw.remaining_wh().value().is_none());
    }

    #[test]
    fn multi_battery_headline_is_unavailable_with_per_battery_list() {
        use pf_core::session::{BatteryMeta, BatteryStaticEntry};
        let meta = BatteryMeta {
            full_mwh: Some(80000.0),
            design_mwh: Some(42000.0),
            battery_index: Some(0),
            battery_count: Some(2),
            batteries: vec![
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
            ],
            ..Default::default()
        };
        let h = headline_health(&meta);
        // Aggregate 80000/42000 must never be paired: headline unavailable.
        assert!(h.value.is_none());
        assert_eq!(h.reason.as_deref(), Some("multi-battery: see per-battery"));
        assert_eq!(h.per_battery.len(), 2);
        assert!((h.per_battery[0].1.unwrap() - 38700.0 / 42000.0).abs() < 1e-9);
        assert!((h.per_battery[1].1.unwrap() - 38000.0 / 40000.0).abs() < 1e-9);
    }

    #[test]
    fn single_battery_keeps_full_over_design() {
        use pf_core::session::BatteryMeta;
        let meta = BatteryMeta {
            full_mwh: Some(38700.0),
            design_mwh: Some(42000.0),
            battery_index: Some(0),
            battery_count: Some(1),
            batteries: Vec::new(),
            ..Default::default()
        };
        let h = headline_health(&meta);
        assert!((h.value.unwrap() - 38700.0 / 42000.0).abs() < 1e-9);
        assert!(h.reason.is_none());
        assert_eq!(per_battery_health(Some(10.0), Some(0.0)), None);
        assert_eq!(per_battery_health(None, Some(10.0)), None);
    }
}
