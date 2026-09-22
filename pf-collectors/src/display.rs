//! Display collector: live brightness (WMI), modes (EnumDisplaySettings),
//! HDR state (DisplayConfig), and change events for the timeline.
//!
//! No admin required. Display on/off power state needs WMI eventing/ETW
//! (planned); display *power* needs a brightness calibration experiment
//! (planned) and is reported Unavailable, never guessed.

use crate::collector::{Collector, CollectorError};
use pf_core::telemetry::{Clock, ClockStamp, FieldMeta, Provenance, escape_json};
#[cfg(windows)]
use std::collections::HashMap;

// ------------------------------------------------------------ user32 GDI ---

/// DISPLAY_DEVICEW (wingdi.h). Must be 840 bytes.
///
/// SAFETY: repr(C) POD mirroring wingdi.h. Size is asserted at runtime in
/// query_hdr/read and in tests; a mismatch degrades gracefully instead of
/// misparsing EnumDisplayDevices output.
#[cfg(windows)]
#[repr(C)]
struct DisplayDevice {
    cb: u32,
    name: [u16; 32],
    string: [u16; 128],
    flags: u32,
    id: [u16; 128],
    key: [u16; 128],
}

/// DEVMODEW (wingdi.h). Must be 220 bytes (verified live: correct 1920x1200@60 read).
///
/// SAFETY: repr(C) POD; size asserted at runtime + in tests, degrades
/// gracefully on mismatch.
#[cfg(windows)]
#[repr(C)]
struct DevMode {
    devname: [u16; 32],
    spec: u16,
    drv: u16,
    size: u16,
    extra: u16,
    fields: u32,
    px: i32,
    py: i32,
    orient: u32,
    fixed: u32,
    color: u16,
    duplex: u16,
    yres: u16,
    ttopt: u16,
    collate: u16,
    form: [u16; 32],
    logpix: u16,
    bpp: u32,
    w: u32,
    h: u32,
    mflags: u32,
    freq: u32,
    icm: u32,
    intent: u32,
    media: u32,
    dither: u32,
    r1: u32,
    r2: u32,
    panw: u32,
    panh: u32,
}

#[cfg(windows)]
const ENUM_CURRENT_SETTINGS: u32 = 0xFFFF_FFFF;
#[cfg(windows)]
const DD_ATTACHED: u32 = 0x1;
#[cfg(windows)]
const DD_PRIMARY: u32 = 0x4;
#[cfg(windows)]
const DD_MIRROR: u32 = 0x8;

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumDisplayDevicesW(
        device: *const u16,
        devnum: u32,
        display: *mut DisplayDevice,
        flags: u32,
    ) -> i32;
    fn EnumDisplaySettingsExW(
        device: *const u16,
        mode: u32,
        devmode: *mut DevMode,
        flags: u32,
    ) -> i32;
    fn GetDisplayConfigBufferSizes(flags: u32, num_path: *mut u32, num_mode: *mut u32) -> i32;
    fn QueryDisplayConfig(
        flags: u32,
        num_path: *mut u32,
        paths: *mut u8,
        num_mode: *mut u32,
        modes: *mut u8,
        topology: *mut u32,
    ) -> i32;
    fn DisplayConfigGetDeviceInfo(packet: *mut u8) -> i32;
}

#[cfg(windows)]
const QDC_ONLY_ACTIVE_PATHS: u32 = 2;
/// DISPLAYCONFIG_PATH_INFO is 72 bytes; target adapter LUID at byte 20,
/// target id at byte 28 (verified against header layout).
#[cfg(windows)]
const PATH_INFO_SIZE: usize = 72;
#[cfg(windows)]
const PATH_TARGET_ADAPTER_OFF: usize = 20;
#[cfg(windows)]
const PATH_TARGET_ID_OFF: usize = 28;
/// DISPLAYCONFIG_MODE_INFO is 64 bytes (16 header + 48 union).
#[cfg(windows)]
const MODE_INFO_SIZE: usize = 64;
#[cfg(windows)]
const DEVINFO_GET_ADVANCED_COLOR: u32 = 0xFFFF_FFFE; // -2
#[cfg(windows)]
const ADV_COLOR_PACKET_SIZE: u32 = 24;

#[cfg(windows)]
fn decode_utf16(raw: &[u16]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    raw[..end]
        .iter()
        .map(|&c| char::from_u32(c as u32).unwrap_or('\u{FFFD}'))
        .collect()
}

// ---------------------------------------------------------- raw COM WMI ---

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct ComGuid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

#[cfg(windows)]
const CLSID_WBEM_LOCATOR: ComGuid = ComGuid {
    d1: 0x4590F811,
    d2: 0x1D3A,
    d3: 0x11D0,
    d4: [0x89, 0x1F, 0x00, 0xAA, 0x00, 0x4B, 0x2E, 0x24],
};
#[cfg(windows)]
const IID_IWBEM_LOCATOR: ComGuid = ComGuid {
    d1: 0xDC12A687,
    d2: 0x737F,
    d3: 0x11CF,
    d4: [0x88, 0x4D, 0x00, 0xAA, 0x00, 0x4B, 0x2E, 0x24],
};

#[cfg(windows)]
const VT_RELEASE: usize = 2;
#[cfg(windows)]
const VT_CONNECT_SERVER: usize = 3;
#[cfg(windows)]
const VT_EXEC_QUERY: usize = 20;
// IEnumWbemClassObject is Reset(3), Next(4), ... — NOT the IEnum-style
// Next-first order. Slot 3 once cost us a WBEM_E_INVALID_OPERATION.
#[cfg(windows)]
const VT_ENUM_NEXT: usize = 4;
#[cfg(windows)]
const VT_OBJECT_GET: usize = 4;

/// VARIANT prefix used for WMI property reads (oleaut32.h). 16 bytes.
///
/// SAFETY: repr(C) POD; only vt/value are touched, VariantInit/Clear own
/// the payload. Size asserted in tests.
#[cfg(windows)]
#[repr(C)]
struct Variant {
    vt: u16,
    reserved: [u16; 3],
    value: u64,
}

#[cfg(windows)]
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: usize, cocreate: u32) -> i32;
    fn CoInitializeSecurity(
        desc: usize,
        auth_svc: i32,
        auth_svc_list: usize,
        reserved1: usize,
        authn_level: u32,
        imp_level: u32,
        auth_list: usize,
        capabilities: u32,
        reserved3: usize,
    ) -> i32;
    fn CoCreateInstance(
        rclsid: *const ComGuid,
        outer: usize,
        clsctx: u32,
        riid: *const ComGuid,
        obj: *mut usize,
    ) -> i32;
    fn CoSetProxyBlanket(
        proxy: usize,
        authn_svc: u32,
        authz_svc: u32,
        server: usize,
        authn_level: u32,
        imp_level: u32,
        auth_info: usize,
        capabilities: u32,
    ) -> i32;
}

#[cfg(windows)]
#[link(name = "oleaut32")]
unsafe extern "system" {
    fn SysAllocString(s: *const u16) -> usize;
    fn SysFreeString(s: usize);
    fn SysStringLen(s: usize) -> u32;
    fn VariantInit(v: *mut Variant);
    fn VariantClear(v: *mut Variant) -> i32;
}

#[cfg(windows)]
unsafe fn vslot(obj: usize, index: usize) -> usize {
    unsafe {
        let vt = *(obj as *const usize);
        *((vt as *const usize).add(index))
    }
}

#[cfg(windows)]
unsafe fn com_release(obj: usize) {
    unsafe {
        let f: unsafe extern "system" fn(usize) -> u32 =
            std::mem::transmute(vslot(obj, VT_RELEASE));
        f(obj);
    }
}

#[cfg(windows)]
fn bstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Debug, Clone, Default)]
struct BrightnessSensor {
    instance: String,
    brightness: u8,
    levels: u32,
}

/// Query ROOT\WMI WmiMonitorBrightness via raw COM. Any failure degrades
/// to Err (brightness sensor Unavailable); modes/HDR are independent.
#[cfg(windows)]
fn query_brightness() -> Result<Vec<BrightnessSensor>, String> {
    // SAFETY: raw COM against WMI; vtable indices are the stable
    // IWbemLocator/IWbemServices/IEnumWbemClassObject/IWbemClassObject
    // layout; every object released, HRESULTs checked.
    unsafe {
        // COM is initialized once per process and deliberately never
        // uninitialized (collector lifetime == process lifetime).
        let rc = CoInitializeEx(0, 0);
        if rc != 0 && rc != 1 && rc as u32 != 0x80010106 {
            return Err(format!("CoInitializeEx failed: {rc:#X}"));
        }
        // Standard WMI blanket; failure is non-fatal (some hosts work
        // without it, and ConnectServer will report properly).
        CoInitializeSecurity(0, -1, 0, 0, 0, 3, 0, 0, 0);

        let mut locator: usize = 0;
        let rc = CoCreateInstance(&CLSID_WBEM_LOCATOR, 0, 1, &IID_IWBEM_LOCATOR, &mut locator);
        if rc != 0 || locator == 0 {
            return Err(format!("WbemLocator failed: {rc:#X}"));
        }
        let connect: unsafe extern "system" fn(
            usize,
            usize,
            usize,
            usize,
            usize,
            i32,
            usize,
            usize,
            *mut usize,
        ) -> i32 = std::mem::transmute(vslot(locator, VT_CONNECT_SERVER));
        let res = bstr("ROOT\\WMI");
        let bres = SysAllocString(res.as_ptr());
        let mut svc: usize = 0;
        let rc = connect(locator, bres, 0, 0, 0, 0, 0, 0, &mut svc);
        SysFreeString(bres);
        com_release(locator);
        if rc != 0 || svc == 0 {
            return Err(format!("WMI ConnectServer ROOT\\WMI failed: {rc:#X}"));
        }
        // Blanket for the proxy; best-effort (checked via ExecQuery).
        CoSetProxyBlanket(svc, 10, 0, 0, 3, 3, 0, 0);

        let exec: unsafe extern "system" fn(usize, usize, usize, i32, usize, *mut usize) -> i32 =
            std::mem::transmute(vslot(svc, VT_EXEC_QUERY));
        let lang = bstr("WQL");
        let blang = SysAllocString(lang.as_ptr());
        let q = bstr("SELECT CurrentBrightness, Levels, InstanceName FROM WmiMonitorBrightness");
        let bq = SysAllocString(q.as_ptr());
        let mut enumerator: usize = 0;
        let rc = exec(svc, blang, bq, 0x30, 0, &mut enumerator);
        SysFreeString(blang);
        SysFreeString(bq);
        com_release(svc);
        if rc != 0 || enumerator == 0 {
            return Err(format!("WMI ExecQuery failed: {rc:#X}"));
        }
        let next: unsafe extern "system" fn(usize, i32, u32, *mut usize, *mut u32) -> i32 =
            std::mem::transmute(vslot(enumerator, VT_ENUM_NEXT));
        let mut out = Vec::new();
        loop {
            let mut obj: usize = 0;
            let mut ret: u32 = 0;
            let rc = next(enumerator, 2000, 1, &mut obj, &mut ret);
            if rc != 0 || ret == 0 || obj == 0 {
                break;
            }
            let get_fn: unsafe extern "system" fn(
                usize,
                usize,
                i32,
                *mut Variant,
                *mut u32,
                *mut u32,
            ) -> i32 = std::mem::transmute(vslot(obj, VT_OBJECT_GET));
            let read_prop = |name: &str| -> Option<(u16, u64)> {
                let w = bstr(name);
                let bw = SysAllocString(w.as_ptr());
                let mut var = Variant {
                    vt: 0,
                    reserved: [0; 3],
                    value: 0,
                };
                VariantInit(&mut var);
                let rc = get_fn(
                    obj,
                    bw,
                    0,
                    &mut var,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                SysFreeString(bw);
                if rc != 0 {
                    VariantClear(&mut var);
                    return None;
                }
                let r = Some((var.vt, var.value));
                VariantClear(&mut var);
                r
            };
            let bri = read_prop("CurrentBrightness").and_then(|(vt, v)| match vt {
                17 => Some((v & 0xFF) as u8),
                3 | 19 => Some((v as u32).min(100) as u8),
                _ => None,
            });
            let levels = read_prop("Levels").and_then(|(vt, v)| match vt {
                3 | 19 => Some(v as u32),
                17 => Some(v as u32),
                _ => None,
            });
            let instance = read_prop("InstanceName").and_then(|(vt, v)| {
                if vt != 8 || v == 0 {
                    return None;
                }
                let len = SysStringLen(v as usize) as usize;
                if len == 0 || len > 260 {
                    return None;
                }
                let slice = std::slice::from_raw_parts(v as *const u16, len);
                Some(decode_utf16(slice))
            });
            com_release(obj);
            if let (Some(b), Some(inst)) = (bri, instance) {
                out.push(BrightnessSensor {
                    instance: inst,
                    brightness: b,
                    levels: levels.unwrap_or(0),
                });
            }
        }
        com_release(enumerator);
        if out.is_empty() {
            return Err("WMI returned no brightness instances".to_string());
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------ data ---

#[derive(Debug, Clone, Default)]
pub struct DisplayInfo {
    pub name: String,
    pub primary: bool,
    pub width: u32,
    pub height: u32,
    pub freq_hz: u32,
    pub bpp: u32,
    pub brightness: Option<u8>,
    pub brightness_levels: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct HdrTarget {
    pub adapter_low: u32,
    pub adapter_high: u32,
    pub target_id: u32,
    pub supported: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DisplaySample {
    pub displays: Vec<DisplayInfo>,
    pub unmatched_sensors: Vec<(String, u8)>,
    pub hdr: Vec<HdrTarget>,
    pub hdr_error: Option<String>,
    pub changes: Vec<String>,
}

/// Match a WMI brightness instance (DISPLAY\AUOA5AB\...) to an EnumDisplay
/// monitor id (MONITOR\AUOA5AB\{...}\0000) on the middle hardware token.
pub fn monitor_token(id: &str) -> String {
    id.split('\\').nth(1).unwrap_or("").to_string()
}

pub fn match_sensor(monitor_id: &str, sensor_instance: &str) -> bool {
    let a = monitor_token(monitor_id);
    let b = monitor_token(sensor_instance);
    !a.is_empty() && a.eq_ignore_ascii_case(&b)
}

/// Diff previous against current display state; returns timeline-grade
/// change descriptions. Pure and tested.
pub fn diff_state(
    prev_modes: &[(String, u32, u32, u32)],
    prev_bri: &[(String, u8)],
    prev_monitors: usize,
    prev_hdr_enabled: usize,
    cur: &DisplaySample,
) -> Vec<String> {
    let mut changes = Vec::new();
    if cur.displays.len() != prev_monitors {
        changes.push(format!(
            "monitor count {} -> {}",
            prev_monitors,
            cur.displays.len()
        ));
    }
    for d in &cur.displays {
        if let Some((_, w, h, f)) = prev_modes.iter().find(|(n, _, _, _)| n == &d.name)
            && (*w != d.width || *h != d.height || *f != d.freq_hz)
        {
            changes.push(format!(
                "mode {} {w}x{h}@{f}Hz -> {}x{}@{}Hz",
                d.name, d.width, d.height, d.freq_hz
            ));
        }
        if let Some(b) = d.brightness
            && let Some((_, pb)) = prev_bri.iter().find(|(n, _)| n == &d.name)
            && *pb != b
        {
            changes.push(format!("brightness {} {pb}% -> {b}%", d.name));
        }
    }
    let hdr_now = cur.hdr.iter().filter(|t| t.enabled).count();
    if hdr_now != prev_hdr_enabled {
        changes.push(format!(
            "HDR enabled targets {prev_hdr_enabled} -> {hdr_now}"
        ));
    }
    changes
}

/// Decode the DISPLAYCONFIG advanced-color value DWORD: bit0 supported,
/// bit1 enabled.
pub fn decode_adv_color(value: u32) -> (bool, bool) {
    (value & 1 != 0, value & 2 != 0)
}

pub struct DisplayCollector {
    #[cfg(windows)]
    prev_modes: HashMap<String, (u32, u32, u32)>,
    #[cfg(windows)]
    prev_bri: HashMap<String, u8>,
    #[cfg(windows)]
    prev_monitors: usize,
    #[cfg(windows)]
    prev_hdr_enabled: usize,
    #[cfg(windows)]
    ever_read: bool,
}

impl DisplayCollector {
    pub fn new() -> Self {
        DisplayCollector {
            #[cfg(windows)]
            prev_modes: HashMap::new(),
            #[cfg(windows)]
            prev_bri: HashMap::new(),
            #[cfg(windows)]
            prev_monitors: 0,
            #[cfg(windows)]
            prev_hdr_enabled: 0,
            #[cfg(windows)]
            ever_read: false,
        }
    }

    pub fn read(&mut self) -> Result<DisplaySample, String> {
        #[cfg(windows)]
        // SAFETY: EnumDisplay/DisplayConfig FFI with checked returns and
        // correctly-sized structs (sizes asserted in tests + verified live).
        unsafe {
            let mut displays = Vec::new();
            let mut i = 0u32;
            loop {
                let mut dd = std::mem::zeroed::<DisplayDevice>();
                dd.cb = std::mem::size_of::<DisplayDevice>() as u32;
                if EnumDisplayDevicesW(std::ptr::null(), i, &mut dd, 0) == 0 {
                    break;
                }
                i += 1;
                if dd.flags & DD_MIRROR != 0 || dd.flags & DD_ATTACHED == 0 {
                    continue;
                }
                let adapter = decode_utf16(&dd.name);
                let primary = dd.flags & DD_PRIMARY != 0;
                let mut dm = std::mem::zeroed::<DevMode>();
                dm.size = std::mem::size_of::<DevMode>() as u16;
                let mode_ok = if adapter.is_empty() {
                    EnumDisplaySettingsExW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut dm, 0)
                } else {
                    let wname: Vec<u16> =
                        adapter.encode_utf16().chain(std::iter::once(0)).collect();
                    EnumDisplaySettingsExW(wname.as_ptr(), ENUM_CURRENT_SETTINGS, &mut dm, 0)
                };
                if mode_ok == 0 {
                    continue;
                }
                // Monitor devices under this adapter for stable names.
                let mut mon_id = String::new();
                if !adapter.is_empty() {
                    let wname: Vec<u16> =
                        adapter.encode_utf16().chain(std::iter::once(0)).collect();
                    let mut j = 0u32;
                    loop {
                        let mut md = std::mem::zeroed::<DisplayDevice>();
                        md.cb = std::mem::size_of::<DisplayDevice>() as u32;
                        if EnumDisplayDevicesW(wname.as_ptr(), j, &mut md, 0) == 0 {
                            break;
                        }
                        j += 1;
                        let id = decode_utf16(&md.id);
                        if !id.is_empty() {
                            mon_id = id;
                            break; // first monitor names the display
                        }
                    }
                }
                displays.push(DisplayInfo {
                    name: if mon_id.is_empty() { adapter } else { mon_id },
                    primary,
                    width: dm.w,
                    height: dm.h,
                    freq_hz: dm.freq,
                    bpp: dm.bpp,
                    brightness: None,
                    brightness_levels: None,
                });
            }

            // Brightness sensors (independent: WMI may fail while modes work).
            let mut unmatched = Vec::new();
            match query_brightness() {
                Ok(sensors) => {
                    for s in &sensors {
                        let mut hit = false;
                        for d in displays.iter_mut() {
                            if match_sensor(&d.name, &s.instance) {
                                d.brightness = Some(s.brightness);
                                d.brightness_levels = Some(s.levels);
                                hit = true;
                                break;
                            }
                        }
                        if !hit {
                            unmatched.push((s.instance.clone(), s.brightness));
                        }
                    }
                }
                Err(e) => {
                    unmatched.push((format!("wmi-error: {e}"), 0));
                }
            }

            // HDR per active DisplayConfig target (independent, defensive).
            let (hdr, hdr_error) = Self::query_hdr();

            let mut sample = DisplaySample {
                displays,
                unmatched_sensors: unmatched,
                hdr,
                hdr_error,
                changes: Vec::new(),
            };
            if self.ever_read {
                let pm: Vec<(String, u32, u32, u32)> = self
                    .prev_modes
                    .iter()
                    .map(|(k, v)| (k.clone(), v.0, v.1, v.2))
                    .collect();
                let pb: Vec<(String, u8)> =
                    self.prev_bri.iter().map(|(k, v)| (k.clone(), *v)).collect();
                sample.changes =
                    diff_state(&pm, &pb, self.prev_monitors, self.prev_hdr_enabled, &sample);
            }
            self.prev_modes = sample
                .displays
                .iter()
                .map(|d| (d.name.clone(), (d.width, d.height, d.freq_hz)))
                .collect();
            self.prev_bri = sample
                .displays
                .iter()
                .filter_map(|d| d.brightness.map(|b| (d.name.clone(), b)))
                .collect();
            self.prev_monitors = sample.displays.len();
            self.prev_hdr_enabled = sample.hdr.iter().filter(|t| t.enabled).count();
            self.ever_read = true;
            Ok(sample)
        }
        #[cfg(not(windows))]
        {
            Err("Windows required".to_string())
        }
    }

    #[cfg(windows)]
    fn query_hdr() -> (Vec<HdrTarget>, Option<String>) {
        // SAFETY: DisplayConfig byte-buffer protocol; PATH_INFO (72B) and
        // MODE_INFO (64B) sizes mirror wingdi.h DISPLAYCONFIG_PATH_INFO /
        // DISPLAYCONFIG_MODE_INFO. All return codes checked; every offset
        // read is bounds-checked and degrades to skipping the target (never
        // a panic or misparse). Multi-byte fields are LE on the wire, so
        // from_le/to_le are used explicitly for portability.
        unsafe {
            // Runtime ABI guard: fail gracefully if our struct sizes drift.
            if std::mem::size_of::<DisplayDevice>() != 840 || std::mem::size_of::<DevMode>() != 220
            {
                return (
                    Vec::new(),
                    Some("display FFI layout mismatch (DisplayDevice/DevMode)".to_string()),
                );
            }
            let mut paths: u32 = 0;
            let mut modes: u32 = 0;
            if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes) != 0
                || paths == 0
            {
                return (
                    Vec::new(),
                    Some("GetDisplayConfigBufferSizes failed".to_string()),
                );
            }
            let mut path_buf = vec![0u8; paths as usize * PATH_INFO_SIZE];
            let mut mode_buf = vec![0u8; modes as usize * MODE_INFO_SIZE];
            let mut np = paths;
            let mut nm = modes;
            if QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut np,
                path_buf.as_mut_ptr(),
                &mut nm,
                mode_buf.as_mut_ptr(),
                std::ptr::null_mut(),
            ) != 0
            {
                return (Vec::new(), Some("QueryDisplayConfig failed".to_string()));
            }
            let mut out = Vec::new();
            let mut skipped = 0u32;
            for i in 0..np as usize {
                let base = i * PATH_INFO_SIZE;
                // Bounds-checked LE load; None skips the target gracefully.
                let get_u32 = |off: usize| -> Option<u32> {
                    let s = path_buf.get(base + off..base + off + 4)?;
                    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
                };
                let (Some(alo), Some(tid), Some(ahi)) = (
                    get_u32(PATH_TARGET_ADAPTER_OFF),
                    get_u32(PATH_TARGET_ID_OFF),
                    get_u32(PATH_TARGET_ADAPTER_OFF + 4),
                ) else {
                    skipped += 1;
                    continue;
                };
                // DISPLAYCONFIG_DEVICE_INFO_HEADER (20B) + value DWORD.
                let mut packet = [0u8; 24];
                packet[0..4].copy_from_slice(&DEVINFO_GET_ADVANCED_COLOR.to_le_bytes());
                packet[4..8].copy_from_slice(&ADV_COLOR_PACKET_SIZE.to_le_bytes());
                packet[8..12].copy_from_slice(&alo.to_le_bytes());
                packet[12..16].copy_from_slice(&ahi.to_le_bytes());
                packet[16..20].copy_from_slice(&tid.to_le_bytes());
                if DisplayConfigGetDeviceInfo(packet.as_mut_ptr()) != 0 {
                    // Per-target failure skips one target, not all (SDR
                    // panels reject the advanced-color query entirely).
                    skipped += 1;
                    continue;
                }
                let v = u32::from_le_bytes([packet[20], packet[21], packet[22], packet[23]]);
                let (supported, enabled) = decode_adv_color(v);
                out.push(HdrTarget {
                    adapter_low: alo,
                    adapter_high: ahi,
                    target_id: tid,
                    supported,
                    enabled,
                });
            }
            if out.is_empty() && skipped > 0 {
                (
                    out,
                    Some(format!(
                        "advanced-color query rejected on all {skipped} target(s) (likely SDR panel)"
                    )),
                )
            } else {
                (out, None)
            }
        }
    }

    /// Render a previously-read sample (collect once, reuse).
    pub fn format_json(
        sample: &DisplaySample,
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        use pf_core::telemetry::{Provenance, UnavailKind, json_num};
        // WMI failure (recorded in unmatched_sensors) is transient;
        // a healthy WMI stack with no matching sensor is Unsupported.
        let wmi_failed = sample
            .unmatched_sensors
            .iter()
            .any(|(s, _)| s.starts_with("wmi-error"));
        let mut displays = String::new();
        for (i, d) in sample.displays.iter().enumerate() {
            if i > 0 {
                displays.push(',');
            }
            let bri = json_num(
                d.brightness.map(|b| b as f64),
                Provenance::Measured,
                if wmi_failed {
                    UnavailKind::TransientError
                } else {
                    UnavailKind::Unsupported
                },
                if wmi_failed {
                    "WMI brightness query failed"
                } else {
                    "no WMI sensor matched"
                },
            );
            displays.push_str(&format!(
                "{{\"name\":\"{}\",\"primary\":{},\"width\":{},\"height\":{},\
                 \"freq_hz\":{},\"bpp\":{},\"brightness_pct\":{bri}}}",
                escape_json(&d.name),
                d.primary,
                d.width,
                d.height,
                d.freq_hz,
                d.bpp,
            ));
        }
        let mut sensors = String::new();
        for (i, (inst, b)) in sample.unmatched_sensors.iter().enumerate() {
            if i > 0 {
                sensors.push(',');
            }
            sensors.push_str(&format!(
                "{{\"instance\":\"{}\",\"brightness\":{b}}}",
                escape_json(inst)
            ));
        }
        let mut hdr = String::new();
        for (i, t) in sample.hdr.iter().enumerate() {
            if i > 0 {
                hdr.push(',');
            }
            hdr.push_str(&format!(
                "{{\"adapter\":\"{:#X}/{:#X}\",\"target\":{},\"supported\":{},\"enabled\":{}}}",
                t.adapter_low, t.adapter_high, t.target_id, t.supported, t.enabled
            ));
        }
        let hdr_err = match &sample.hdr_error {
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
            "{{\"collector\":\"display\",\"wall_ms\":{},\"mono_ms\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{},\
             \"displays\":[{displays}],\"unmatched_sensors\":[{sensors}],\
             \"hdr_targets\":[{hdr}],\"hdr_error\":{hdr_err},\"changes\":[{changes}]}}",
            stamp.wall_millis, stamp.mono_millis, t_start_ms, t_end_ms,
        )
    }
}

impl Default for DisplayCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for DisplayCollector {
    fn name(&self) -> &'static str {
        "display"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                name: "display.mode",
                unit: "px/Hz",
                source: "EnumDisplayDevices + EnumDisplaySettingsEx",
                provenance: Provenance::Measured,
                interval_ms: 2_000,
                requires_admin: false,
                notes: "Resolution, refresh, bpp, primary per attached display",
            },
            FieldMeta {
                name: "display.brightness_pct",
                unit: "%",
                source: "WMI ROOT\\WMI WmiMonitorBrightness (raw COM)",
                provenance: Provenance::Measured,
                interval_ms: 2_000,
                requires_admin: false,
                notes: "Live sensor (not policy); matched to displays by hardware token",
            },
            FieldMeta {
                name: "display.monitors",
                unit: "list",
                source: "EnumDisplayDevices monitor enumeration",
                provenance: Provenance::Measured,
                interval_ms: 2_000,
                requires_admin: false,
                notes: "Count changes become connect/disconnect timeline events",
            },
            FieldMeta {
                name: "display.hdr",
                unit: "bool/target",
                source: "DisplayConfig advanced-color info per active target",
                provenance: Provenance::Measured,
                interval_ms: 2_000,
                requires_admin: false,
                notes: "Supported + enabled; target IDs, not friendly names; queried every display tick, not a slow tier",
            },
            FieldMeta {
                name: "display.changes",
                unit: "events",
                source: "prev-state diff (mode/brightness/count/HDR)",
                provenance: Provenance::Derived,
                interval_ms: 2_000,
                requires_admin: false,
                notes: "Feed the event timeline",
            },
            FieldMeta {
                name: "display.power_on",
                unit: "bool",
                source: "WMI brightness events / ETW — planned",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "Panel power state is not pollable without eventing",
            },
            FieldMeta {
                name: "display.power_w",
                unit: "W",
                source: "brightness calibration experiment — planned",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "Estimated only after a laptop-specific brightness curve is measured",
            },
        ]
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let t0 = clock.stamp();
        let sample = self.read().map_err(|e| CollectorError::new("display", e))?;
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

    #[test]
    fn monitor_token_matching() {
        assert_eq!(monitor_token("MONITOR\\AUOA5AB\\{x}\\0000"), "AUOA5AB");
        assert_eq!(
            monitor_token("DISPLAY\\AUOA5AB\\5&ddbb1e4&0&UID256_0"),
            "AUOA5AB"
        );
        assert!(match_sensor(
            "MONITOR\\AUOA5AB\\{x}\\0000",
            "DISPLAY\\AUOA5AB\\5&ddbb1e4&0&UID256_0"
        ));
        assert!(!match_sensor(
            "MONITOR\\XXXX\\{x}\\0000",
            "DISPLAY\\AUOA5AB\\y"
        ));
        assert!(!match_sensor("", ""));
        assert!(!match_sensor("NOSEP", "ALS ONOSEP"));
    }

    #[test]
    fn change_diff() {
        let cur = DisplaySample {
            displays: vec![DisplayInfo {
                name: "AUOA5AB".to_string(),
                primary: true,
                width: 1920,
                height: 1200,
                freq_hz: 60,
                bpp: 32,
                brightness: Some(10),
                brightness_levels: Some(101),
            }],
            unmatched_sensors: vec![],
            hdr: vec![],
            hdr_error: None,
            changes: vec![],
        };
        // First read baselines everything: pass matching prev = no changes.
        let pm = vec![("AUOA5AB".to_string(), 1920, 1200, 60)];
        let pb = vec![("AUOA5AB".to_string(), 10u8)];
        assert!(diff_state(&pm, &pb, 1, 0, &cur).is_empty());
        // Brightness + mode + count + HDR changes all detected.
        let cur2 = DisplaySample {
            displays: vec![],
            unmatched_sensors: vec![],
            hdr: vec![HdrTarget {
                adapter_low: 1,
                adapter_high: 0,
                target_id: 0,
                supported: true,
                enabled: true,
            }],
            hdr_error: None,
            changes: vec![],
        };
        let c = diff_state(&pm, &pb, 1, 0, &cur2);
        assert!(c.iter().any(|s| s.contains("monitor count 1 -> 0")));
        assert!(c.iter().any(|s| s.contains("HDR enabled targets 0 -> 1")));
        let mut cur3 = cur.clone();
        cur3.displays[0].brightness = Some(40);
        cur3.displays[0].freq_hz = 120;
        let c3 = diff_state(&pm, &pb, 1, 0, &cur3);
        assert!(
            c3.iter()
                .any(|s| s.contains("brightness AUOA5AB 10% -> 40%"))
        );
        assert!(c3.iter().any(|s| s.contains("120Hz")));
    }

    #[test]
    fn adv_color_bits() {
        assert_eq!(decode_adv_color(0), (false, false));
        assert_eq!(decode_adv_color(1), (true, false));
        assert_eq!(decode_adv_color(3), (true, true));
    }

    #[cfg(windows)]
    #[test]
    fn struct_sizes_match_win32() {
        assert_eq!(std::mem::size_of::<DevMode>(), 220);
        assert_eq!(std::mem::size_of::<DisplayDevice>(), 840);
        assert_eq!(std::mem::size_of::<Variant>(), 16);
        assert_eq!(std::mem::size_of::<ComGuid>(), 16);
    }
}
