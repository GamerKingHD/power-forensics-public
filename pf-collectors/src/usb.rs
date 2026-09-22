//! USB/peripheral collector: present-device inventory per PnP class
//! (SetupDi, no admin), connect/disconnect timeline events from set diffs,
//! and devices with power requirements (DevicePowerEnumDevices).
//!
//! Camera/mic *presence* is enumerable; *in-use* state needs ETW or
//! MediaFoundation session tracking (Unavailable, planned). USB storage
//! throughput is covered by the storage collector (mass-storage devices
//! appear as PhysicalDisks).

use crate::collector::{Collector, CollectorError};
use pf_core::telemetry::{Clock, ClockStamp, FieldMeta, Provenance, escape_json};
#[cfg(windows)]
use std::collections::HashMap;

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct ClassGuid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

#[cfg(windows)]
struct ClassDef {
    key: &'static str,
    guid: ClassGuid,
}

#[cfg(windows)]
const CLASSES: &[ClassDef] = &[
    ClassDef {
        key: "usb",
        guid: ClassGuid {
            d1: 0x36FC9E60,
            d2: 0xC465,
            d3: 0x11CF,
            d4: [0x80, 0x56, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00],
        },
    },
    ClassDef {
        key: "bluetooth",
        guid: ClassGuid {
            d1: 0xE0CBF06C,
            d2: 0xCD8B,
            d3: 0x4647,
            d4: [0xBB, 0x8A, 0x26, 0x3B, 0x43, 0xF0, 0xF9, 0x74],
        },
    },
    ClassDef {
        key: "camera",
        guid: ClassGuid {
            d1: 0xCA3E7AB9,
            d2: 0xB4C3,
            d3: 0x4AE6,
            d4: [0x82, 0x51, 0x57, 0x9E, 0xF9, 0x33, 0x89, 0x0F],
        },
    },
    ClassDef {
        key: "audio",
        guid: ClassGuid {
            d1: 0x4D36E96C,
            d2: 0xE325,
            d3: 0x11CE,
            d4: [0xBF, 0xC1, 0x08, 0x00, 0x2B, 0xE1, 0x03, 0x18],
        },
    },
    ClassDef {
        key: "hid",
        guid: ClassGuid {
            d1: 0x745A17A0,
            d2: 0x74D3,
            d3: 0x11D0,
            d4: [0xB6, 0xFE, 0x00, 0xA0, 0xC9, 0x0F, 0x57, 0xDA],
        },
    },
];

/// SP_DEVINFO_DATA (setupapi.h). cb_size must equal size_of::<DevInfoData>().
///
/// SAFETY: repr(C) POD mirroring setupapi.h. `class_guid` is 16 raw bytes
/// (GUID layout, LE for the first three fields). Size is pointer-width
/// dependent (28 on 32-bit, 32 on 64-bit); `assert_usb_layouts()` guards
/// it at runtime and enumeration degrades to empty on mismatch.
#[cfg(windows)]
#[repr(C)]
struct DevInfoData {
    cb_size: u32,
    class_guid: [u8; 16],
    dev_inst: u32,
    reserved: usize,
}

/// Runtime ABI guard for the hand-rolled SetupDi FFI. Returns false on
/// layout mismatch so callers degrade to empty instead of misparsing.
#[cfg(windows)]
fn assert_usb_layouts() -> bool {
    // SAFETY: repr(C) PODs; SP_DEVINFO_DATA is 28 bytes on 32-bit and 32
    // bytes on 64-bit (ULONG_PTR tail). ClassGuid is always 16 bytes.
    let dev_size = std::mem::size_of::<DevInfoData>();
    let ok_dev = dev_size == 32 || dev_size == 28;
    let ok_guid = std::mem::size_of::<ClassGuid>() == 16;
    ok_dev && ok_guid
}

#[cfg(windows)]
const DIGCF_PRESENT: u32 = 0x2;
#[cfg(windows)]
const SPDRP_DEVICEDESC: u32 = 0;
#[cfg(windows)]
const SPDRP_FRIENDLYNAME: u32 = 12;
#[cfg(windows)]
const DN_STARTED: u32 = 0x8;

#[cfg(windows)]
#[link(name = "setupapi")]
unsafe extern "system" {
    fn SetupDiGetClassDevsW(
        class: *const ClassGuid,
        enumerator: *const u16,
        parent: usize,
        flags: u32,
    ) -> isize;
    fn SetupDiEnumDeviceInfo(set: isize, index: u32, data: *mut DevInfoData) -> i32;
    fn SetupDiGetDeviceRegistryPropertyW(
        set: isize,
        data: *const DevInfoData,
        prop: u32,
        regtype: *mut u32,
        buffer: *mut u8,
        size: u32,
        needed: *mut u32,
    ) -> i32;
    fn SetupDiGetDeviceInstanceIdW(
        set: isize,
        data: *const DevInfoData,
        buffer: *mut u16,
        size: u32,
        needed: *mut u32,
    ) -> i32;
    fn SetupDiDestroyDeviceInfoList(set: isize) -> i32;
}

#[cfg(windows)]
#[link(name = "cfgmgr32")]
unsafe extern "system" {
    fn CM_Get_DevNode_Status(status: *mut u32, problem: *mut u32, devinst: u32, flags: u32) -> u32;
}

#[cfg(windows)]
#[link(name = "powrprof")]
unsafe extern "system" {
    fn DevicePowerEnumDevices(
        index: u32,
        interpretation: u32,
        flags: u32,
        buffer: *mut u8,
        size: *mut u32,
    ) -> u8;
}

#[derive(Debug, Clone, Default)]
pub struct UsbDevice {
    pub instance: String,
    pub friendly: String,
    pub started: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceGroup {
    pub key: String,
    pub devices: Vec<UsbDevice>,
}

#[derive(Debug, Clone, Default)]
pub struct UsbSample {
    pub groups: Vec<DeviceGroup>,
    pub power_devices: Vec<String>,
    pub power_error: Option<String>,
    pub changes: Vec<String>,
}

/// Set diff between consecutive inventories. Pure and tested.
pub fn diff_devices(prev: &HashMap<String, String>, cur: &HashMap<String, String>) -> Vec<String> {
    let mut changes = Vec::new();
    for (id, name) in cur {
        if !prev.contains_key(id) {
            changes.push(format!("connected: {name} [{id}]"));
        }
    }
    for (id, name) in prev {
        if !cur.contains_key(id) {
            changes.push(format!("disconnected: {name} [{id}]"));
        }
    }
    changes.sort();
    changes
}

/// Parse a nul-separated WCHAR device list defensively: keep plausible
/// names only (the record format is undocumented; garbage must not reach
/// the session).
pub fn parse_power_list(buf: &[u8]) -> Vec<String> {
    // Device strings are UTF-16LE on the wire; decode as LE explicitly so
    // parsing is portable (identical on x86/x64, correct on BE hosts).
    let units: Vec<u16> = buf
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut segments = Vec::new();
    let mut cur = String::new();
    for &u in &units {
        if u == 0 {
            segments.push(std::mem::take(&mut cur));
        } else if let Some(c) = char::from_u32(u as u32) {
            cur.push(c);
            if cur.len() > 160 {
                cur.clear();
            }
        }
    }
    segments.push(cur);
    segments
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| {
            s.len() >= 3
                && s.len() <= 128
                && s.chars()
                    .all(|c| c.is_alphanumeric() || " _:;()-.{}\\/".contains(c))
        })
        .take(64)
        .collect()
}

#[cfg(windows)]
fn decode_prop(set: isize, data: &DevInfoData, prop: u32) -> String {
    let mut buf = [0u16; 256];
    let bytes = std::mem::size_of_val(&buf) as u32;
    // SAFETY: stack buffer with exact byte size, return checked.
    let rc = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set,
            data,
            prop,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut u8,
            bytes,
            std::ptr::null_mut(),
        )
    };
    if rc == 0 {
        return String::new();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    buf[..end]
        .iter()
        .map(|&c| char::from_u32(c as u32).unwrap_or('\u{FFFD}'))
        .collect()
}

#[cfg(windows)]
fn enumerate_class(def: &ClassDef) -> Vec<UsbDevice> {
    // SAFETY: SetupDi handle discipline; destroyed on every path.
    // SP_DEVINFO_DATA.cb_size is set from size_of (pointer-width correct);
    // a layout mismatch degrades to empty rather than misparsing.
    if !assert_usb_layouts() {
        return Vec::new();
    }
    unsafe {
        let set = SetupDiGetClassDevsW(&def.guid, std::ptr::null(), 0, DIGCF_PRESENT);
        if set == -1 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut i = 0u32;
        loop {
            let mut data = std::mem::zeroed::<DevInfoData>();
            data.cb_size = std::mem::size_of::<DevInfoData>() as u32;
            if SetupDiEnumDeviceInfo(set, i, &mut data) == 0 {
                break;
            }
            i += 1;
            let mut instance = [0u16; 256];
            let ok = SetupDiGetDeviceInstanceIdW(
                set,
                &data,
                instance.as_mut_ptr(),
                instance.len() as u32,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                continue;
            }
            let end = instance
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(instance.len());
            let id: String = instance[..end]
                .iter()
                .map(|&c| char::from_u32(c as u32).unwrap_or('\u{FFFD}'))
                .collect();
            let friendly = decode_prop(set, &data, SPDRP_FRIENDLYNAME);
            let descr = decode_prop(set, &data, SPDRP_DEVICEDESC);
            let name = if friendly.is_empty() { descr } else { friendly };
            let mut status: u32 = 0;
            let mut problem: u32 = 0;
            let started = CM_Get_DevNode_Status(&mut status, &mut problem, data.dev_inst, 0) == 0
                && status & DN_STARTED != 0;
            out.push(UsbDevice {
                instance: id,
                friendly: name,
                started,
            });
            if out.len() >= 256 {
                break;
            }
        }
        SetupDiDestroyDeviceInfoList(set);
        out
    }
}

#[cfg(windows)]
fn query_power_devices() -> Result<Vec<String>, String> {
    // SAFETY: stack-vec buffer with exact size, return checked.
    unsafe {
        let mut buf = vec![0u8; 8192];
        let mut size = buf.len() as u32;
        let rc = DevicePowerEnumDevices(0, 0, 0x1, buf.as_mut_ptr(), &mut size);
        if rc == 0 {
            return Err("DevicePowerEnumDevices failed".to_string());
        }
        Ok(parse_power_list(&buf))
    }
}

pub struct UsbCollector {
    #[cfg(windows)]
    prev: HashMap<String, String>,
    #[cfg(windows)]
    ever_read: bool,
    #[cfg(windows)]
    tick: u64,
    #[cfg(windows)]
    cached: Option<UsbSample>,
}

/// Device topology changes rarely: enumerate every Nth tick, serve the
/// cache in between (effective interval noted in capabilities).
#[cfg(windows)]
const ENUM_EVERY: u64 = 5;

impl UsbCollector {
    pub fn new() -> Self {
        UsbCollector {
            #[cfg(windows)]
            prev: HashMap::new(),
            #[cfg(windows)]
            ever_read: false,
            #[cfg(windows)]
            tick: 0,
            #[cfg(windows)]
            cached: None,
        }
    }

    pub fn read(&mut self) -> Result<UsbSample, String> {
        #[cfg(windows)]
        {
            self.tick += 1;
            let full = !self.ever_read || self.tick.is_multiple_of(ENUM_EVERY);
            if !full && let Some(cached) = &self.cached {
                let mut sample = cached.clone();
                sample.changes = Vec::new();
                return Ok(sample);
            }
            let mut groups = Vec::new();
            let mut current: HashMap<String, String> = HashMap::new();
            for def in CLASSES {
                let devices = enumerate_class(def);
                for d in &devices {
                    current.insert(format!("{}:{}", def.key, d.instance), d.friendly.clone());
                }
                groups.push(DeviceGroup {
                    key: def.key.to_string(),
                    devices,
                });
            }
            let (power_devices, power_error) = match query_power_devices() {
                Ok(v) => (v, None),
                Err(e) => (Vec::new(), Some(e)),
            };
            let mut changes = Vec::new();
            if self.ever_read {
                changes = diff_devices(&self.prev, &current);
            }
            self.prev = current;
            self.ever_read = true;
            let sample = UsbSample {
                groups,
                power_devices,
                power_error,
                changes,
            };
            self.cached = Some(sample.clone());
            // Return without changes duplication: changes live in this
            // fresh sample; cached copies clear them above.
            Ok(sample)
        }
        #[cfg(not(windows))]
        {
            Err("Windows required".to_string())
        }
    }

    /// Render a previously-read sample (collect once, reuse).
    pub fn format_json(
        sample: &UsbSample,
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        let mut groups = String::new();
        for (gi, g) in sample.groups.iter().enumerate() {
            if gi > 0 {
                groups.push(',');
            }
            let mut devs = String::new();
            for (i, d) in g.devices.iter().enumerate() {
                if i > 0 {
                    devs.push(',');
                }
                devs.push_str(&format!(
                    "{{\"instance\":\"{}\",\"name\":\"{}\",\"started\":{}}}",
                    escape_json(&d.instance),
                    escape_json(&d.friendly),
                    d.started,
                ));
            }
            groups.push_str(&format!("{{\"class\":\"{}\",\"devices\":[{devs}]}}", g.key));
        }
        let mut power = String::new();
        for (i, p) in sample.power_devices.iter().enumerate() {
            if i > 0 {
                power.push(',');
            }
            power.push_str(&format!("\"{}\"", escape_json(p)));
        }
        let power_err = match &sample.power_error {
            Some(e) => format!("\"{}\"", escape_json(e)),
            None => "null".to_string(),
        };
        let mut changes = String::new();
        for (i, c) in sample.changes.iter().enumerate() {
            if i > 0 {
                changes.push(',');
            }
            changes.push_str(&format!("\"{}\"", escape_json(c)));
        }
        format!(
            "{{\"collector\":\"usb\",\"wall_ms\":{},\"mono_ms\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{},\
             \"groups\":[{groups}],\"power_devices\":[{power}],\
             \"power_error\":{power_err},\"changes\":[{changes}]}}",
            stamp.wall_millis, stamp.mono_millis, t_start_ms, t_end_ms,
        )
    }
}

impl Default for UsbCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for UsbCollector {
    fn name(&self) -> &'static str {
        "usb"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                name: "usb.devices",
                unit: "lists",
                source: "SetupDi class enumeration (USB/BT/camera/audio/HID, present only)",
                provenance: Provenance::Measured,
                interval_ms: 150_000,
                requires_admin: false,
                notes: "Instance ID, name, started flag; scheduler ticks at 30 s but enumeration is cached and refreshed every 5th tick (~150 s)",
            },
            FieldMeta {
                name: "usb.changes",
                unit: "events",
                source: "instance-set diff vs previous enumeration",
                provenance: Provenance::Derived,
                interval_ms: 150_000,
                requires_admin: false,
                notes: "Connect/disconnect timeline events with VID/PID in the ID; diff runs on the ~150 s enumeration",
            },
            FieldMeta {
                name: "usb.power_devices",
                unit: "names",
                source: "powrprof DevicePowerEnumDevices (capability mask 0x1)",
                provenance: Provenance::Measured,
                interval_ms: 150_000,
                requires_admin: false,
                notes: "Devices declaring power requirements; record format undocumented, parsed defensively; refreshed with the enumeration cache",
            },
            FieldMeta {
                name: "usb.camera_mic_in_use",
                unit: "bool",
                source: "ETW / MediaFoundation session tracking — planned",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: true,
                notes: "Presence is enumerated; active streaming needs kernel tracing",
            },
            FieldMeta {
                name: "usb.selective_suspend",
                unit: "state",
                source: "USB power framework queries — planned",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "Per-device suspend state is the low-power-states mechanism",
            },
            FieldMeta {
                name: "usb.storage_throughput",
                unit: "B/s",
                source: "covered by the storage collector (mass storage = PhysicalDisk)",
                provenance: Provenance::Derived,
                interval_ms: 0,
                requires_admin: false,
                notes: "Not sampled by this collector (interval 0); no separate USB mass-storage path, join by drive identity later",
            },
        ]
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let t0 = clock.stamp();
        let sample = self.read().map_err(|e| CollectorError::new("usb", e))?;
        let t1 = clock.stamp();
        Ok(Self::format_json(
            &sample,
            t1,
            t0.mono_millis,
            t1.mono_millis,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn device_diff() {
        let prev = map(&[("usb:A", "Mouse"), ("usb:B", "Stick")]);
        let cur = map(&[("usb:B", "Stick"), ("usb:C", "SSD")]);
        let c = diff_devices(&prev, &cur);
        assert_eq!(c.len(), 2);
        assert!(
            c.iter()
                .any(|s| s.contains("disconnected") && s.contains("Mouse"))
        );
        assert!(
            c.iter()
                .any(|s| s.contains("connected") && s.contains("SSD"))
        );
        // Identical sets: silence.
        assert!(diff_devices(&prev, &prev).is_empty());
        // Rename with same ID: no event (identity is the instance ID).
        let renamed = map(&[("usb:A", "Mouse2"), ("usb:B", "Stick")]);
        assert!(diff_devices(&prev, &renamed).is_empty());
    }

    #[test]
    fn power_list_parsing() {
        let mut raw = Vec::new();
        for s in ["SPPSERVICE4", "Device\\Harddisk0"] {
            raw.extend(s.encode_utf16().flat_map(|c| c.to_le_bytes()));
            raw.extend([0, 0]);
        }
        raw.extend([0u8; 64]);
        let v = parse_power_list(&raw);
        assert_eq!(
            v,
            vec!["SPPSERVICE4".to_string(), "Device\\Harddisk0".to_string()]
        );
        assert!(parse_power_list(&[]).is_empty());
        assert!(parse_power_list(&[0u8; 8]).is_empty());
        // Garbage control chars are dropped, not recorded.
        let mut bad = vec![0x01u8, 0x00, 0x02, 0x00, 0x00, 0x00];
        bad.extend([0u8; 10]);
        assert!(parse_power_list(&bad).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn usb_layout_guard() {
        assert!(
            assert_usb_layouts(),
            "SP_DEVINFO_DATA/ClassGuid size mismatch"
        );
    }
}
