//! Static battery data via IOCTL_BATTERY_QUERY_TAG/INFORMATION.
//!
//! `CallNtPowerInformation` gives live capacities but no design capacity,
//! chemistry, temperature, or identifiers. Those come from the ACPI battery
//! device interface (GUID_DEVICE_BATTERY) via DeviceIoControl. No admin
//! required where the interface exists; fully graceful otherwise.

#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

/// GUID_DEVICE_BATTERY {72631E54-78A4-11D0-BCF7-00AA00B7B32A}
#[cfg(windows)]
const GUID_DEVICE_BATTERY: Guid = Guid {
    d1: 0x72631E54,
    d2: 0x78A4,
    d3: 0x11D0,
    d4: [0xBC, 0xF7, 0x00, 0xAA, 0x00, 0xB7, 0xB3, 0x2A],
};

/// CTL_CODE(FILE_DEVICE_BATTERY=0x29, func, METHOD_BUFFERED, FILE_READ_ACCESS)
const IOCTL_BATTERY_QUERY_TAG: u32 = 0x294040; // func 0x10
const IOCTL_BATTERY_QUERY_INFORMATION: u32 = 0x294048; // func 0x12
const BATTERY_TAG_INVALID: u32 = 0;

const LEVEL_INFO: u32 = 0;
const LEVEL_TEMPERATURE: u32 = 2;
const LEVEL_DEVICE_NAME: u32 = 4;
const LEVEL_MFG_DATE: u32 = 5;
const LEVEL_MFG_NAME: u32 = 6;
const LEVEL_UNIQUE_ID: u32 = 7;

#[cfg(windows)]
#[repr(C)]
struct QueryInfo {
    tag: u32,
    level: u32,
    at_rate: i32,
}

/// BATTERY_INFORMATION (winioctl.h). Must be 36 bytes.
///
/// SAFETY: repr(C) POD mirroring the winioctl.h definition. Layout is
/// verified at runtime by `assert_battery_layouts()` (called before any
/// IOCTL) and in tests; a mismatch degrades to Err instead of misparsing.
#[cfg(windows)]
#[repr(C)]
struct BatteryInformation {
    capabilities: u32,
    technology: u8,
    _reserved: [u8; 3],
    chemistry: [u8; 4],
    designed_capacity: u32,
    full_charged_capacity: u32,
    _default_alert1: u32,
    _default_alert2: u32,
    _critical_bias: u32,
    cycle_count: u32,
}

#[cfg(windows)]
#[repr(C)]
struct ManufactureDate {
    day: u8,
    month: u8,
    year: u16,
}

/// Runtime ABI guard for the hand-rolled battery FFI. Returns Err on
/// layout mismatch so callers degrade gracefully instead of misparsing
/// driver output. Expected sizes come from winioctl.h
/// (BATTERY_INFORMATION=36, BATTERY_QUERY_INFORMATION struct=12,
/// BATTERY_MANUFACTURE_DATE=4, GUID=16).
#[cfg(windows)]
fn assert_battery_layouts() -> Result<(), String> {
    // SAFETY: these are repr(C) PODs; any compiler/packing surprise is a
    // hard error here, not silent driver-output corruption downstream.
    let checks = [
        (
            "BatteryInformation",
            std::mem::size_of::<BatteryInformation>(),
            36,
        ),
        ("QueryInfo", std::mem::size_of::<QueryInfo>(), 12),
        ("ManufactureDate", std::mem::size_of::<ManufactureDate>(), 4),
        ("Guid", std::mem::size_of::<Guid>(), 16),
    ];
    for (name, got, want) in checks {
        if got != want {
            return Err(format!(
                "FFI layout mismatch: {name} is {got} bytes, expected {want}"
            ));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn assert_battery_layouts() -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
#[link(name = "cfgmgr32")]
unsafe extern "system" {
    fn CM_Get_Device_Interface_List_SizeW(
        pulen: *mut u32,
        interface_class: *const Guid,
        device_id: *const u16,
        flags: u32,
    ) -> u32;
    fn CM_Get_Device_Interface_ListW(
        interface_class: *const Guid,
        device_id: *const u16,
        buffer: *mut u16,
        buffer_len: u32,
        flags: u32,
    ) -> u32;
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: usize,
        creation: u32,
        flags: u32,
        template: usize,
    ) -> isize;
    fn DeviceIoControl(
        handle: isize,
        code: u32,
        in_buf: *const c_void,
        in_len: u32,
        out_buf: *mut c_void,
        out_len: u32,
        ret_len: *mut u32,
        overlapped: usize,
    ) -> i32;
    fn CloseHandle(handle: isize) -> i32;
}

#[cfg(windows)]
const GENERIC_RW: u32 = 0x8000_0000 | 0x4000_0000;
#[cfg(windows)]
const SHARE_RW: u32 = 0x1 | 0x2;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;

#[derive(Debug, Clone, Default)]
pub struct BatteryStatic {
    pub designed_mwh: Option<u32>,
    pub full_mwh: Option<u32>,
    pub cycle_count: Option<u32>,
    pub chemistry: Option<String>,
    pub technology: Option<u8>,
    pub capabilities: Option<u32>,
    /// Raw ULONG from BatteryTemperature. Units suspected 0.1 K but
    /// UNVERIFIED on this hardware — kept raw, never converted blindly.
    pub temperature_raw: Option<u32>,
    pub device_name: Option<String>,
    pub mfg_name: Option<String>,
    pub unique_id: Option<String>,
    pub mfg_date: Option<(u16, u8, u8)>, // (year, month, day)
}

/// Decode a nul-terminated UTF-16 buffer.
fn decode_wstr(buf: &[u16]) -> Option<String> {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s: String = buf[..end]
        .iter()
        .map(|&c| char::from_u32(c as u32).unwrap_or('\u{FFFD}'))
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Split the battery device-interface multi-sz list into individual paths.
#[cfg(windows)]
fn interface_paths() -> Vec<Vec<u16>> {
    // SAFETY: size query then buffer query, both returns checked.
    unsafe {
        let mut len: u32 = 0;
        if CM_Get_Device_Interface_List_SizeW(&mut len, &GUID_DEVICE_BATTERY, std::ptr::null(), 0)
            != 0
            || len < 2
        {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_Interface_ListW(
            &GUID_DEVICE_BATTERY,
            std::ptr::null(),
            buf.as_mut_ptr(),
            len,
            0,
        ) != 0
        {
            return Vec::new();
        }
        let mut paths = Vec::new();
        let mut i = 0;
        while i < buf.len() && buf[i] != 0 {
            let start = i;
            while i < buf.len() && buf[i] != 0 {
                i += 1;
            }
            let mut p: Vec<u16> = buf[start..i].to_vec();
            p.push(0);
            paths.push(p);
            i += 1; // skip the separator nul
        }
        paths
    }
}

#[cfg(not(windows))]
fn interface_paths() -> Vec<Vec<u16>> {
    Vec::new()
}

/// Number of physical ACPI battery interfaces present.
pub fn count_interfaces() -> usize {
    interface_paths().len()
}

/// Query every physical battery interface as (index, static). Failures are
/// skipped per interface so a partially responsive multi-battery system still
/// reports the batteries that answered.
pub fn query_all() -> Vec<(usize, BatteryStatic)> {
    interface_paths()
        .into_iter()
        .enumerate()
        .filter_map(|(i, p)| query_path(&p).ok().map(|s| (i, s)))
        .collect()
}

/// Open one battery interface and read its static metadata.
#[cfg(windows)]
fn query_path(path: &[u16]) -> Result<BatteryStatic, String> {
    // Hardened FFI: verify struct layouts before touching the driver.
    assert_battery_layouts()?;
    // SAFETY: FFI out-params with checked CR_SUCCESS (0) returns throughout.
    unsafe {
        let handle = CreateFileW(path.as_ptr(), GENERIC_RW, SHARE_RW, 0, OPEN_EXISTING, 0, 0);
        if handle == -1 {
            return Err(format!(
                "cannot open battery device: {}",
                std::io::Error::last_os_error()
            ));
        }

        struct Guard(isize);
        impl Drop for Guard {
            fn drop(&mut self) {
                // SAFETY: valid handle from CreateFileW, teardown path.
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let _guard = Guard(handle);

        let query_ioctl = |code: u32, input: &[u8], output: &mut [u8]| -> Result<u32, String> {
            let mut ret: u32 = 0;
            let rc = DeviceIoControl(
                handle,
                code,
                input.as_ptr() as *const c_void,
                input.len() as u32,
                output.as_mut_ptr() as *mut c_void,
                output.len() as u32,
                &mut ret,
                0,
            );
            if rc == 0 {
                return Err(format!(
                    "IOCTL {code:#X} failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(ret)
        };

        // Tag query takes a DWORD wait hint (LE on the wire for IOCTLs).
        let wait: u32 = 0;
        let mut tag_out = [0u8; 4];
        let tag_bytes = wait.to_le_bytes();
        query_ioctl(IOCTL_BATTERY_QUERY_TAG, &tag_bytes, &mut tag_out)?;
        let tag = u32::from_le_bytes(tag_out);
        if tag == BATTERY_TAG_INVALID {
            return Err("battery tag invalid (battery may be absent)".to_string());
        }

        let mut info = BatteryInformation {
            capabilities: 0,
            technology: 0,
            _reserved: [0; 3],
            chemistry: [0; 4],
            designed_capacity: 0,
            full_charged_capacity: 0,
            _default_alert1: 0,
            _default_alert2: 0,
            _critical_bias: 0,
            cycle_count: 0,
        };
        let ask = |level: u32, out: &mut [u8]| -> Result<u32, String> {
            let qi = QueryInfo {
                tag,
                level,
                at_rate: 0,
            };
            // SAFETY: repr(C) POD struct read as bytes for the input buffer.
            let bytes: &[u8] = std::slice::from_raw_parts(
                &qi as *const QueryInfo as *const u8,
                std::mem::size_of::<QueryInfo>(),
            );
            query_ioctl(IOCTL_BATTERY_QUERY_INFORMATION, bytes, out)
        };

        // SAFETY: BatteryInformation is repr(C) POD, sized exactly.
        let info_bytes: &mut [u8] = std::slice::from_raw_parts_mut(
            &mut info as *mut BatteryInformation as *mut u8,
            std::mem::size_of::<BatteryInformation>(),
        );
        let got = ask(LEVEL_INFO, info_bytes)?;
        if (got as usize) < std::mem::size_of::<BatteryInformation>() {
            // Observed on ASUS ACPI batteries: success with 0 bytes, on
            // every level including STATUS. The driver stack does not
            // implement query IOCTLs; not a caller bug (verified byte
            // for byte against independent probes).
            return Err(format!(
                "battery driver returned success with {got} bytes (query IOCTLs not implemented by this driver)"
            ));
        }

        let mut out = BatteryStatic {
            designed_mwh: (info.designed_capacity > 0).then_some(info.designed_capacity),
            full_mwh: (info.full_charged_capacity > 0).then_some(info.full_charged_capacity),
            cycle_count: Some(info.cycle_count),
            chemistry: decode_wstr(&info.chemistry.map(|b| b as u16))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            technology: Some(info.technology),
            capabilities: Some(info.capabilities),
            ..Default::default()
        };

        // Optional details: each degrades independently.
        let mut raw4 = [0u8; 4];
        if ask(LEVEL_TEMPERATURE, &mut raw4).is_ok() {
            out.temperature_raw = Some(u32::from_le_bytes(raw4));
        }
        let wstr = |level: u32| -> Option<String> {
            let mut buf = [0u16; 128];
            // SAFETY: u16 buffer viewed as bytes for the IOCTL output.
            let bytes: &mut [u8] =
                std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len() * 2);
            ask(level, bytes).ok()?;
            decode_wstr(&buf)
        };
        out.device_name = wstr(LEVEL_DEVICE_NAME);
        out.mfg_name = wstr(LEVEL_MFG_NAME);
        out.unique_id = wstr(LEVEL_UNIQUE_ID);
        let mut date = ManufactureDate {
            day: 0,
            month: 0,
            year: 0,
        };
        // SAFETY: repr(C) POD struct sized exactly.
        let date_bytes: &mut [u8] = std::slice::from_raw_parts_mut(
            &mut date as *mut ManufactureDate as *mut u8,
            std::mem::size_of::<ManufactureDate>(),
        );
        if ask(LEVEL_MFG_DATE, date_bytes).is_ok() && date.year > 1990 && date.year < 2100 {
            out.mfg_date = Some((date.year, date.month, date.day));
        }
        Ok(out)
    }
}

#[cfg(not(windows))]
fn query_path(_path: &[u16]) -> Result<BatteryStatic, String> {
    Err("Windows required".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_codes_match_ctl_code_macro() {
        // CTL_CODE(0x29, f, METHOD_BUFFERED=0, FILE_READ_ACCESS=1)
        let ctl = |f: u32| (0x29u32 << 16) | (1 << 14) | (f << 2);
        assert_eq!(IOCTL_BATTERY_QUERY_TAG, ctl(0x10));
        assert_eq!(IOCTL_BATTERY_QUERY_INFORMATION, ctl(0x12));
        assert_eq!(BATTERY_TAG_INVALID, 0);
    }

    #[test]
    fn wstr_decode_trims_and_stops_at_nul() {
        assert_eq!(
            decode_wstr(&[
                'A' as u16, 'S' as u16, 'U' as u16, 'S' as u16, 0, 'X' as u16
            ]),
            Some("ASUS".to_string())
        );
        assert_eq!(decode_wstr(&[0, 0]), None);
        assert_eq!(decode_wstr(&[]), None);
        assert_eq!(decode_wstr(&[' ' as u16, 0]), None);
    }

    #[test]
    fn chemistry_bytes_decode_like_lion() {
        let chem = *b"LION";
        let s = decode_wstr(&chem.map(|b| b as u16)).unwrap();
        assert_eq!(s, "LION");
    }

    #[cfg(windows)]
    #[test]
    fn battery_information_layout_is_36_bytes() {
        assert_eq!(std::mem::size_of::<BatteryInformation>(), 36);
        assert_eq!(std::mem::size_of::<QueryInfo>(), 12);
        assert_eq!(std::mem::size_of::<ManufactureDate>(), 4);
        assert_eq!(std::mem::size_of::<Guid>(), 16);
        assert_battery_layouts().expect("runtime layout guard must pass");
    }

    #[test]
    fn le_tag_roundtrip() {
        let wait: u32 = 0x1234_5678;
        let b = wait.to_le_bytes();
        assert_eq!(u32::from_le_bytes(b), wait);
    }
}
