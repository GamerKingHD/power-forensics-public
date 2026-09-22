//! Network collector: adapter inventory (iphlpapi GetIfTable2), Wi-Fi
//! signal/SSID (wlanapi), per-adapter throughput (PDH), and oper-state
//! change events for the timeline.
//!
//! No admin required. Per-process traffic needs ETW (planned, admin);
//! NDIS adapter power state is driver-dependent (Unavailable, not guessed).

use crate::collector::{Collector, CollectorError};
#[cfg(windows)]
use crate::cpu::InitRetry;
use crate::pdh::PdhQuery;
use pf_core::telemetry::{Clock, ClockStamp, FieldMeta, Provenance, escape_json};
#[cfg(windows)]
use std::collections::HashMap;

// ------------------------------------------------------------- iphlpapi ---

// Authoritative MIB_IF_ROW2 / MIB_IF_TABLE2 come from the `windows` crate
// (Win32_NetworkManagement_IpHelper). The previous hand-derived struct with
// a fixed 1352-byte stride was an unverifiable ABI assumption; the OS owns
// the layout and alignment padding before/between rows.
#[cfg(windows)]
use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};

#[cfg(windows)]
fn guid_bytes(g: &windows::core::GUID) -> [u8; 16] {
    // GUID wire/memory order is little-endian for data1..data3 (RFC 4122
    // §4.1.2 / Windows GUID layout). Use LE explicitly so the byte join
    // matches on every host; identical to NE on x86/x64, correct on BE.
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&g.data1.to_le_bytes());
    b[4..6].copy_from_slice(&g.data2.to_le_bytes());
    b[6..8].copy_from_slice(&g.data3.to_le_bytes());
    b[8..16].copy_from_slice(&g.data4);
    b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterClass {
    Wifi,
    Ethernet,
    Bluetooth,
    Loopback,
    Other,
}

impl AdapterClass {
    pub fn as_str(self) -> &'static str {
        match self {
            AdapterClass::Wifi => "wifi",
            AdapterClass::Ethernet => "ethernet",
            AdapterClass::Bluetooth => "bluetooth",
            AdapterClass::Loopback => "loopback",
            AdapterClass::Other => "other",
        }
    }
}

pub fn classify_adapter(iftype: u32, descr_lower: &str) -> AdapterClass {
    if iftype == 24 || descr_lower.contains("loopback") {
        AdapterClass::Loopback
    } else if iftype == 71 {
        AdapterClass::Wifi
    } else if descr_lower.contains("bluetooth") {
        AdapterClass::Bluetooth
    } else if iftype == 6 {
        AdapterClass::Ethernet
    } else {
        AdapterClass::Other
    }
}

pub fn oper_name(oper: u32) -> String {
    match oper {
        1 => "Up".to_string(),
        2 => "Down".to_string(),
        3 => "Testing".to_string(),
        4 => "Unknown".to_string(),
        5 => "Dormant".to_string(),
        6 => "NotPresent".to_string(),
        7 => "LowerLayerDown".to_string(),
        n => format!("oper-{n}"),
    }
}

pub fn media_name(media: u32) -> String {
    match media {
        0 => "Unknown".to_string(),
        1 => "Connected".to_string(),
        2 => "Disconnected".to_string(),
        n => format!("media-{n}"),
    }
}

pub fn iftype_name(iftype: u32) -> String {
    match iftype {
        6 => "ethernet".to_string(),
        24 => "loopback".to_string(),
        71 => "wifi".to_string(),
        131 => "tunnel".to_string(),
        n => format!("type-{n}"),
    }
}

#[derive(Debug, Clone, Default)]
pub struct NetAdapter {
    pub alias: String,
    pub descr: String,
    pub guid: [u8; 16],
    pub class: String,
    pub iftype: String,
    pub oper: u32,
    pub oper_name: String,
    pub media: String,
    pub admin_up: bool,
    pub tx_mbps: f64,
    pub rx_mbps: f64,
    pub in_octets: u64,
    pub out_octets: u64,
}

#[cfg(windows)]
fn decode_utf16(raw: &[u16]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    raw[..end]
        .iter()
        .map(|&c| char::from_u32(c as u32).unwrap_or('\u{FFFD}'))
        .collect()
}

#[cfg(windows)]
fn read_table() -> Result<Vec<NetAdapter>, String> {
    // SAFETY: GetIfTable2 allocates the table; rows are read from the OS-owned
    // `Table` flexible array using its authoritative type; the table is freed
    // on the path below.
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        let rc = GetIfTable2(&mut table);
        if rc.0 != 0 || table.is_null() {
            return Err(format!("GetIfTable2 failed: {}", rc.0));
        }
        let n = (*table).NumEntries as usize;
        let rows = (*table).Table.as_ptr();
        let mut out = Vec::new();
        for i in 0..n.min(128) {
            let row = &*rows.add(i);
            let descr = decode_utf16(&row.Description);
            let alias = decode_utf16(&row.Alias);
            let class = classify_adapter(row.Type, &descr.to_lowercase());
            out.push(NetAdapter {
                alias,
                descr,
                guid: guid_bytes(&row.InterfaceGuid),
                class: class.as_str().to_string(),
                iftype: iftype_name(row.Type),
                oper: row.OperStatus.0 as u32,
                oper_name: oper_name(row.OperStatus.0 as u32),
                media: media_name(row.MediaConnectState.0 as u32),
                admin_up: row.AdminStatus.0 == 1,
                tx_mbps: row.TransmitLinkSpeed as f64 / 1_000_000.0,
                rx_mbps: row.ReceiveLinkSpeed as f64 / 1_000_000.0,
                in_octets: row.InOctets,
                out_octets: row.OutOctets,
            });
        }
        FreeMibTable(table as *const core::ffi::c_void);
        Ok(out)
    }
}

// -------------------------------------------------------------- wlanapi ---
// Typed windows-crate bindings (Win32_NetworkManagement_WiFi, windows 0.62):
// WlanOpenHandle / WlanEnumInterfaces / WlanQueryInterface operate on the
// authoritative WLAN_INTERFACE_INFO_LIST / WLAN_CONNECTION_ATTRIBUTES
// structs. No hand-derived strides or byte offsets; the OS owns the layout.

#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows::Win32::NetworkManagement::WiFi::{
    WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO, WLAN_INTERFACE_INFO_LIST, WlanCloseHandle,
    WlanEnumInterfaces, WlanFreeMemory, WlanOpenHandle, WlanQueryInterface,
    wlan_interface_state_connected, wlan_intf_opcode_current_connection,
};

#[derive(Debug, Clone, Default)]
pub struct WifiNet {
    pub guid: [u8; 16],
    pub descr: String,
    pub state: u32,
    pub ssid: String,
    pub signal_pct: Option<u8>,
}

pub fn wlan_state_name(state: u32) -> &'static str {
    match state {
        0 => "not-ready",
        1 => "connected",
        2 => "adhoc",
        3 => "disconnecting",
        4 => "disconnected",
        5 => "associating",
        6 => "discovering",
        7 => "authenticating",
        _ => "unknown",
    }
}

/// Decode an 802.11 SSID payload (not necessarily UTF-8). Pure and tested.
/// `len` is bounds-checked against `bytes`; trailing NUL padding is trimmed
/// so overlong lengths on zero-padded buffers degrade to the real content
/// instead of misparsing (embedded NULs are preserved).
pub fn decode_ssid_bytes(len: u32, bytes: &[u8; 32]) -> String {
    let n = (len as usize).min(32);
    let mut end = n;
    while end > 0 && bytes[end - 1] == 0 {
        end -= 1;
    }
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

/// Decode WLAN signal quality (0-100%). Out-of-range values degrade to
/// None instead of a bogus percentage. Pure and tested.
pub fn decode_signal(quality: u32) -> Option<u8> {
    if quality <= 100 {
        Some(quality as u8)
    } else {
        None
    }
}

/// Decode one typed WLAN_CONNECTION_ATTRIBUTES record: (ssid, signal).
/// None when not connected or fields are implausible; never misparses.
#[cfg(windows)]
pub fn parse_wlan_conn(attr: &WLAN_CONNECTION_ATTRIBUTES) -> Option<(String, Option<u8>)> {
    if attr.isState != wlan_interface_state_connected {
        return None; // not connected
    }
    let ssid_len = attr.wlanAssociationAttributes.dot11Ssid.uSSIDLength;
    if ssid_len > 32 {
        return None;
    }
    let ssid = decode_ssid_bytes(ssid_len, &attr.wlanAssociationAttributes.dot11Ssid.ucSSID);
    let signal = decode_signal(attr.wlanAssociationAttributes.wlanSignalQuality);
    Some((ssid, signal))
}

/// Project one typed WLAN_INTERFACE_INFO row to (guid bytes, descr, state).
/// Pure projection over the authoritative struct; no offsets.
#[cfg(windows)]
pub fn wlan_entry_fields(info: &WLAN_INTERFACE_INFO) -> ([u8; 16], String, u32) {
    (
        guid_bytes(&info.InterfaceGuid),
        decode_utf16(&info.strInterfaceDescription),
        info.isState.0 as u32,
    )
}

#[cfg(windows)]
fn query_wifi() -> Vec<WifiNet> {
    // SAFETY: wlanapi handle discipline via verified windows-crate
    // bindings; every allocation freed with WlanFreeMemory, handle closed
    // with WlanCloseHandle; all return codes checked. Flexible-array rows
    // are read via the authoritative WLAN_INTERFACE_INFO type (never byte
    // offsets); count is clamped to 64 and every pointer is null-checked.
    // WlanQueryInterface payloads are size-checked against
    // size_of::<WLAN_CONNECTION_ATTRIBUTES>() before reinterpretation and
    // degrade to (empty ssid, None signal) on mismatch.
    unsafe {
        let mut handle = HANDLE::default();
        let mut negotiated: u32 = 0;
        if WlanOpenHandle(2, None, &mut negotiated, &mut handle) != 0 || handle.is_invalid() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut list_ptr: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        if WlanEnumInterfaces(handle, None, &mut list_ptr) == 0 && !list_ptr.is_null() {
            let count = (*list_ptr).dwNumberOfItems as usize;
            let rows = (*list_ptr).InterfaceInfo.as_ptr();
            for i in 0..count.min(64) {
                let info: &WLAN_INTERFACE_INFO = &*rows.add(i);
                let (guid, descr, state) = wlan_entry_fields(info);
                let mut ssid = String::new();
                let mut signal = None;
                if info.isState == wlan_interface_state_connected {
                    let mut size: u32 = 0;
                    let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
                    if WlanQueryInterface(
                        handle,
                        &info.InterfaceGuid,
                        wlan_intf_opcode_current_connection,
                        None,
                        &mut size,
                        &mut data,
                        None,
                    ) == 0
                        && !data.is_null()
                    {
                        if size as usize >= std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
                            let attr = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                            if let Some((s, q)) = parse_wlan_conn(attr) {
                                ssid = s;
                                signal = q;
                            }
                        }
                        WlanFreeMemory(data);
                    }
                }
                out.push(WifiNet {
                    guid,
                    descr,
                    state,
                    ssid,
                    signal_pct: signal,
                });
            }
            WlanFreeMemory(list_ptr as *const core::ffi::c_void);
        }
        WlanCloseHandle(handle, None);
        out
    }
}

// ------------------------------------------------------------ throughput ---

#[derive(Debug, Clone, Default)]
pub struct Throughput {
    pub instance: String,
    pub rx_bps: Option<f64>,
    pub tx_bps: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct NetSample {
    pub adapters: Vec<NetAdapter>,
    pub throughput: Vec<Throughput>,
    pub wifi: Vec<WifiNet>,
    /// Authoritative totals: sum over non-loopback interfaces with a usable
    /// counter. None when no such counter existed — absence never collapses
    /// to a 0 total. This is what format_json serializes.
    pub total_rx_bps_opt: Option<f64>,
    pub total_tx_bps_opt: Option<f64>,
    pub changes: Vec<String>,
    /// True when the last counter-set rebuild failed and a previous interface
    /// topology is still in use. Values may be valid, but the topology model is
    /// not current; the failure reason is preserved.
    pub topology_stale: bool,
    pub topology_error: Option<String>,
}

/// Sum non-loopback interface counters. A missing counter is skipped; a
/// present zero stays a zero. Returns None when no interface produced a
/// usable counter at all (missing is not zero), and the covered/total
/// counts so callers can express partial coverage.
pub fn sum_non_loopback(
    throughput: &[Throughput],
    adapters: &[NetAdapter],
) -> (Option<f64>, Option<f64>, usize, usize) {
    let mut usable = 0usize;
    let mut total_rx = 0.0;
    let mut total_tx = 0.0;
    for t in throughput {
        let loopback = match match_adapter(&t.instance, adapters) {
            Some((i, _)) => adapters[i].class == AdapterClass::Loopback.as_str(),
            None => t.instance.to_lowercase().contains("loopback"),
        };
        if loopback {
            continue;
        }
        match (t.rx_bps, t.tx_bps) {
            (Some(rx), Some(tx)) => {
                usable += 1;
                total_rx += rx;
                total_tx += tx;
            }
            // A one-sided counter (rate still warming up) contributes its
            // known half; an all-missing row contributes nothing.
            (Some(rx), None) => {
                usable += 1;
                total_rx += rx;
            }
            (None, Some(tx)) => {
                usable += 1;
                total_tx += tx;
            }
            (None, None) => {}
        }
    }
    let rx = (usable > 0).then_some(total_rx);
    let tx = (usable > 0).then_some(total_tx);
    (rx, tx, usable, throughput.len())
}

/// Best-effort PDH instance -> adapter join. PDH lowercases driver
/// descriptions, so compare case-insensitively; exact match beats fuzzy.
pub fn match_adapter(instance: &str, adapters: &[NetAdapter]) -> Option<(usize, bool)> {
    let inst = instance.to_lowercase();
    for (i, a) in adapters.iter().enumerate() {
        if a.descr.to_lowercase() == inst {
            return Some((i, true));
        }
    }
    for (i, a) in adapters.iter().enumerate() {
        let d = a.descr.to_lowercase();
        if !d.is_empty() && (d.contains(&inst) || inst.contains(&d)) {
            return Some((i, false));
        }
    }
    None
}

/// Human rate for the dashboard. Pure and tested.
pub fn human_bps(bps: f64) -> String {
    if bps < 1000.0 {
        format!("{bps:.0}B/s")
    } else if bps < 1_000_000.0 {
        format!("{:.1}KB/s", bps / 1000.0)
    } else {
        format!("{:.1}MB/s", bps / 1_000_000.0)
    }
}

/// Display-only noise reduction: NDIS filter/miniport/tunnel rows triple
/// the adapter list without owning traffic. The JSON session keeps every
/// row; only the dashboard hides these (unless one ever carries traffic).
pub fn is_virtual_noise(alias: &str, descr: &str) -> bool {
    const MARKERS: &[&str] = &[
        "filter",
        "qos",
        "wfp",
        "miniport",
        "teredo",
        "6to4",
        "ip-https",
        "kernel debugger",
        "tunnel",
        "pseudo-interface",
    ];
    let hay = format!("{alias} {descr}").to_lowercase();
    MARKERS.iter().any(|m| hay.contains(m))
}

pub struct NetCollector {
    #[cfg(windows)]
    pdh: Option<PdhQuery>,
    #[cfg(windows)]
    tp: Vec<(String, usize, usize)>,
    #[cfg(windows)]
    init_retry: InitRetry,
    #[cfg(windows)]
    fresh: bool,
    #[cfg(windows)]
    tick: u64,
    #[cfg(windows)]
    prev_state: HashMap<String, (u32, String)>,
    #[cfg(windows)]
    ever_read: bool,
    /// Adapter inventory (GetIfTable2 + wlanapi) changes rarely; refresh
    /// every 30 reads while throughput stays per-read. Set by scheduler
    /// cadence, not by this counter, in normal operation.
    #[cfg(windows)]
    inv_tick: u64,
    #[cfg(windows)]
    cached_adapters: Vec<NetAdapter>,
    #[cfg(windows)]
    cached_wifi: Vec<WifiNet>,
    /// True when the last counter-set rebuild failed while an older set is
    /// still being read: values may be valid but the interface/counter model
    /// is not current. Cleared on the next successful rebuild.
    #[cfg(windows)]
    topology_stale: bool,
    /// Why the last rebuild failed (preserved while stale).
    #[cfg(windows)]
    topology_error: Option<String>,
}

#[cfg(windows)]
const REBUILD_EVERY: u64 = 60;

impl NetCollector {
    pub fn new() -> Self {
        NetCollector {
            #[cfg(windows)]
            pdh: None,
            #[cfg(windows)]
            tp: Vec::new(),
            #[cfg(windows)]
            init_retry: InitRetry::new(),
            #[cfg(windows)]
            fresh: false,
            #[cfg(windows)]
            tick: 0,
            #[cfg(windows)]
            prev_state: HashMap::new(),
            #[cfg(windows)]
            ever_read: false,
            #[cfg(windows)]
            inv_tick: 0,
            #[cfg(windows)]
            cached_adapters: Vec::new(),
            #[cfg(windows)]
            cached_wifi: Vec::new(),
            #[cfg(windows)]
            topology_stale: false,
            #[cfg(windows)]
            topology_error: None,
        }
    }

    #[cfg(windows)]
    fn ensure_init(&mut self) -> Result<(), String> {
        if self.pdh.is_some() {
            return Ok(());
        }
        if !self.init_retry.may_attempt(self.tick) {
            return Err(self
                .init_retry
                .cached()
                .unwrap_or_else(|| "initialization retry pending".to_string()));
        }
        match self.build_counters() {
            Ok(()) => {
                self.init_retry.note_success();
                Ok(())
            }
            Err(e) => Err(self.init_retry.note_failure(self.tick, e)),
        }
    }

    /// Apply the outcome of a periodic counter-set rebuild. On success the
    /// new instance/handle table is installed and staleness cleared; on
    /// failure the previous table is RETAINED (its counters stay readable)
    /// while the topology is marked stale with the reason, so old values can
    /// never be presented with fresh-topology semantics. Narrow and pure so
    /// the transition is testable without a real PDH failure.
    #[cfg(windows)]
    fn apply_rebuild(&mut self, outcome: Result<Vec<(String, usize, usize)>, String>) {
        match outcome {
            Ok(tp) => {
                self.tp = tp;
                self.topology_stale = false;
                self.topology_error = None;
            }
            Err(e) => {
                self.topology_stale = true;
                self.topology_error = Some(e);
            }
        }
    }

    #[cfg(windows)]
    fn build_counters(&mut self) -> Result<(), String> {
        let pdh = PdhQuery::open()?;
        let mut tp = Vec::new();
        if let Ok(insts) = PdhQuery::enum_instances("Network Interface") {
            for inst in insts {
                let hr = pdh.add(&format!("\\Network Interface({inst})\\Bytes Received/sec"));
                let hs = pdh.add(&format!("\\Network Interface({inst})\\Bytes Sent/sec"));
                if hr != 0 || hs != 0 {
                    tp.push((inst, hr, hs));
                }
            }
        }
        if tp.is_empty() {
            return Err("no Network Interface counters could be added".to_string());
        }
        pdh.collect()?;
        std::thread::sleep(std::time::Duration::from_millis(150));
        pdh.collect()?;
        self.pdh = Some(pdh);
        self.tp = tp;
        self.fresh = true;
        Ok(())
    }

    pub fn read(&mut self) -> Result<NetSample, String> {
        #[cfg(windows)]
        {
            // Advance before init so a failed attempt still counts down the
            // retry backoff.
            self.tick = self.tick.wrapping_add(1);
            self.ensure_init()?;
            if self.tick.is_multiple_of(REBUILD_EVERY) {
                // Swap only on success; keep the old set when rebuilding fails,
                // but mark the topology stale and preserve the reason so the
                // values are never presented as a current interface model.
                let mut probe = NetCollector::new();
                match probe.build_counters() {
                    Ok(()) => {
                        self.pdh = probe.pdh;
                        self.apply_rebuild(Ok(probe.tp));
                    }
                    Err(e) => {
                        self.apply_rebuild(Err(e));
                    }
                }
            }
            let pdh = self.pdh.as_ref().ok_or("PDH query not initialized")?;
            if self.fresh {
                self.fresh = false;
            } else {
                pdh.collect()?;
            }
            self.inv_tick += 1;
            if self.inv_tick == 1 || self.inv_tick.is_multiple_of(30) {
                self.cached_adapters = read_table().unwrap_or_default();
                self.cached_wifi = query_wifi();
            }
            let adapters = self.cached_adapters.clone();
            let wifi = self.cached_wifi.clone();
            let mut throughput = Vec::with_capacity(self.tp.len());
            for (inst, hr, hs) in &self.tp {
                throughput.push(Throughput {
                    instance: inst.clone(),
                    rx_bps: pdh.read_double(*hr),
                    tx_bps: pdh.read_double(*hs),
                });
            }
            // Totals over non-loopback traffic. None (not 0.0) when no
            // interface produced a usable counter: missing is not zero.
            let (total_rx, total_tx, _, _) = sum_non_loopback(&throughput, &adapters);
            // Oper/media transitions become timeline events.
            let mut changes = Vec::new();
            if self.ever_read {
                for a in &adapters {
                    let cur = (a.oper, a.media.clone());
                    if let Some(prev) = self.prev_state.get(&a.alias)
                        && *prev != cur
                    {
                        changes.push(format!(
                            "{} {} {} -> {} {}",
                            a.alias,
                            prev.1,
                            oper_name(prev.0),
                            a.media,
                            a.oper_name
                        ));
                    }
                }
            }
            self.prev_state = adapters
                .iter()
                .map(|a| (a.alias.clone(), (a.oper, a.media.clone())))
                .collect();
            self.ever_read = true;
            Ok(NetSample {
                adapters,
                throughput,
                wifi,
                total_rx_bps_opt: total_rx,
                total_tx_bps_opt: total_tx,
                changes,
                topology_stale: self.topology_stale,
                topology_error: self.topology_error.clone(),
            })
        }
        #[cfg(not(windows))]
        {
            Err("Windows required".to_string())
        }
    }

    /// Render a previously-read sample (collect once, reuse).
    pub fn format_json(
        sample: &NetSample,
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        fn opt(v: Option<f64>) -> String {
            pf_core::telemetry::json_num(
                v,
                pf_core::telemetry::Provenance::Measured,
                pf_core::telemetry::UnavailKind::NotSampled,
                "counter not ready",
            )
        }
        let mut adapters = String::new();
        for (i, a) in sample.adapters.iter().enumerate() {
            if i > 0 {
                adapters.push(',');
            }
            adapters.push_str(&format!(
                "{{\"alias\":\"{}\",\"descr\":\"{}\",\"guid\":\"{}\",\"class\":\"{}\",\
                 \"iftype\":\"{}\",\"oper\":{},\"oper_name\":\"{}\",\"media\":\"{}\",\
                 \"admin_up\":{},\"tx_mbps\":{:.1},\"rx_mbps\":{:.1},\
                 \"in_octets\":{},\"out_octets\":{}}}",
                escape_json(&a.alias),
                escape_json(&a.descr),
                a.guid
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
                a.class,
                a.iftype,
                a.oper,
                a.oper_name,
                a.media,
                a.admin_up,
                a.tx_mbps,
                a.rx_mbps,
                a.in_octets,
                a.out_octets,
            ));
        }
        let mut tp = String::new();
        for (i, t) in sample.throughput.iter().enumerate() {
            if i > 0 {
                tp.push(',');
            }
            tp.push_str(&format!(
                "{{\"instance\":\"{}\",\"rx_bps\":{},\"tx_bps\":{}}}",
                escape_json(&t.instance),
                opt(t.rx_bps),
                opt(t.tx_bps),
            ));
        }
        let mut wifi = String::new();
        for (i, w) in sample.wifi.iter().enumerate() {
            if i > 0 {
                wifi.push(',');
            }
            let sig = match w.signal_pct {
                Some(s) => format!("{{\"v\":{s},\"p\":\"measured\"}}"),
                None => {
                    "{\"v\":null,\"p\":\"unavailable\",\"reason\":\"not connected\"}".to_string()
                }
            };
            wifi.push_str(&format!(
                "{{\"descr\":\"{}\",\"state\":\"{}\",\"ssid\":\"{}\",\"signal_pct\":{sig}}}",
                escape_json(&w.descr),
                wlan_state_name(w.state),
                escape_json(&w.ssid),
            ));
        }
        let mut changes = String::new();
        for (i, c) in sample.changes.iter().enumerate() {
            if i > 0 {
                changes.push(',');
            }
            changes.push_str(&format!("\"{}\"", escape_json(c)));
        }
        let topo_err = match &sample.topology_error {
            Some(e) => format!("\"{}\"", escape_json(e)),
            None => "null".to_string(),
        };
        format!(
            "{{\"collector\":\"net\",\"wall_ms\":{},\"mono_ms\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{},\
             \"adapters\":[{adapters}],\"throughput\":[{tp}],\"wifi\":[{wifi}],\
             \"total_rx_bps\":{},\"total_tx_bps\":{},\"changes\":[{changes}],\
             \"topology_stale\":{},\"topology_error\":{topo_err}}}",
            stamp.wall_millis,
            stamp.mono_millis,
            t_start_ms,
            t_end_ms,
            pf_core::telemetry::json_num(
                sample.total_rx_bps_opt,
                pf_core::telemetry::Provenance::Derived,
                pf_core::telemetry::UnavailKind::NotSampled,
                "no interface counters available",
            ),
            pf_core::telemetry::json_num(
                sample.total_tx_bps_opt,
                pf_core::telemetry::Provenance::Derived,
                pf_core::telemetry::UnavailKind::NotSampled,
                "no interface counters available",
            ),
            sample.topology_stale,
        )
    }
}

impl Default for NetCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for NetCollector {
    fn name(&self) -> &'static str {
        "net"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                name: "net.adapter.inventory",
                unit: "adapters",
                source: "iphlpapi GetIfTable2",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "Alias, type, oper/media state, link speeds, cumulative octets; cached and re-read every 30 reads (30 s at the 1 s base tick), not per tick",
            },
            FieldMeta {
                name: "net.throughput_bps",
                unit: "B/s",
                source: "PDH Network Interface(*) Bytes Received/Sent per sec",
                provenance: Provenance::Measured,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Joined to inventory by case-insensitive description match",
            },
            FieldMeta {
                name: "net.totals_bps",
                unit: "B/s",
                source: "sum over non-loopback interfaces",
                provenance: Provenance::Derived,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Correlation signal, not an energy meter; Unavailable (not 0) when no interface counter is usable",
            },
            FieldMeta {
                name: "net.topology_stale",
                unit: "bool",
                source: "PDH counter-set rebuild outcome",
                provenance: Provenance::Measured,
                interval_ms: 60_000,
                requires_admin: false,
                notes: "True when the periodic counter-set rebuild failed and a previous interface topology is still in use; values may be valid but the topology is not current. Cleared on the next successful rebuild. topology_error preserves the reason",
            },
            FieldMeta {
                name: "net.wifi",
                unit: "%/ssid",
                source: "wlanapi signal quality + SSID for connected interfaces",
                provenance: Provenance::Measured,
                interval_ms: 30_000,
                requires_admin: false,
                notes: "Joined to adapters by interface GUID; refreshed with the 30-read inventory cache",
            },
            FieldMeta {
                name: "net.changes",
                unit: "events",
                source: "oper/media transitions vs previous read",
                provenance: Provenance::Derived,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Connect/disconnect timeline events",
            },
            FieldMeta {
                name: "net.proc_bps",
                unit: "B/s",
                source: "ETW network events — planned, needs admin",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: true,
                notes: "Task Manager's per-app network needs kernel tracing",
            },
            FieldMeta {
                name: "net.adapter_power",
                unit: "state",
                source: "NDIS PM state — driver-dependent, planned",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "No generic Windows API; link speed is the available proxy",
            },
        ]
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let t0 = clock.stamp();
        let sample = self.read().map_err(|e| CollectorError::new("net", e))?;
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
    fn names() {
        assert_eq!(oper_name(1), "Up");
        assert_eq!(oper_name(7), "LowerLayerDown");
        assert_eq!(oper_name(99), "oper-99");
        assert_eq!(media_name(1), "Connected");
        assert_eq!(media_name(2), "Disconnected");
        assert_eq!(iftype_name(6), "ethernet");
        assert_eq!(iftype_name(71), "wifi");
        assert_eq!(wlan_state_name(1), "connected");
        assert_eq!(wlan_state_name(4), "disconnected");
    }

    #[test]
    fn classification() {
        assert_eq!(classify_adapter(71, "mediatek wifi"), AdapterClass::Wifi);
        assert_eq!(
            classify_adapter(6, "intel ethernet"),
            AdapterClass::Ethernet
        );
        assert_eq!(
            classify_adapter(6, "bluetooth device (personal area network)"),
            AdapterClass::Bluetooth
        );
        assert_eq!(classify_adapter(24, "loopback"), AdapterClass::Loopback);
        assert_eq!(classify_adapter(131, "teredo"), AdapterClass::Other);
    }

    #[test]
    fn wlan_ssid_decoding() {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(b"Home");
        assert_eq!(decode_ssid_bytes(4, &bytes), "Home");
        assert_eq!(decode_ssid_bytes(0, &bytes), "");
        // Overlong length clamps to 32, never reads out of bounds.
        assert_eq!(decode_ssid_bytes(99, &bytes).len(), 4);
        let mut full = [b'A'; 32];
        full[0..4].copy_from_slice(b"Home");
        assert_eq!(decode_ssid_bytes(u32::MAX, &full).len(), 32);
    }

    #[test]
    fn wlan_signal_decoding() {
        assert_eq!(decode_signal(80), Some(80));
        assert_eq!(decode_signal(0), Some(0));
        assert_eq!(decode_signal(100), Some(100));
        assert_eq!(decode_signal(101), None);
        assert_eq!(decode_signal(u32::MAX), None);
    }

    #[cfg(windows)]
    #[test]
    fn wlan_conn_typed() {
        use windows::Win32::NetworkManagement::WiFi::{
            WLAN_CONNECTION_ATTRIBUTES, wlan_interface_state_connected,
            wlan_interface_state_disconnected,
        };
        let mut attr = WLAN_CONNECTION_ATTRIBUTES {
            isState: wlan_interface_state_connected,
            ..Default::default()
        };
        attr.wlanAssociationAttributes.dot11Ssid.uSSIDLength = 4;
        attr.wlanAssociationAttributes.dot11Ssid.ucSSID[..4].copy_from_slice(b"Home");
        attr.wlanAssociationAttributes.wlanSignalQuality = 80;
        assert_eq!(parse_wlan_conn(&attr), Some(("Home".to_string(), Some(80))));
        attr.isState = wlan_interface_state_disconnected;
        assert_eq!(parse_wlan_conn(&attr), None);
        attr.isState = wlan_interface_state_connected;
        attr.wlanAssociationAttributes.dot11Ssid.uSSIDLength = 99;
        assert_eq!(parse_wlan_conn(&attr), None);
        attr.wlanAssociationAttributes.dot11Ssid.uSSIDLength = 4;
        attr.wlanAssociationAttributes.wlanSignalQuality = 101;
        assert_eq!(parse_wlan_conn(&attr), Some(("Home".to_string(), None)));
    }

    #[cfg(windows)]
    #[test]
    fn wlan_entry_projection() {
        use windows::Win32::NetworkManagement::WiFi::{
            WLAN_INTERFACE_INFO, wlan_interface_state_connected,
        };
        let mut info = WLAN_INTERFACE_INFO::default();
        for (i, c) in "WiFi".encode_utf16().enumerate() {
            info.strInterfaceDescription[i] = c;
        }
        info.isState = wlan_interface_state_connected;
        let (_, descr, state) = wlan_entry_fields(&info);
        assert_eq!(descr, "WiFi");
        assert_eq!(state, 1);
    }

    #[test]
    fn adapter_matching() {
        let adapters = vec![
            NetAdapter {
                descr: "MediaTek Wi-Fi 6E MT7902 Wireless LAN Card".to_string(),
                ..Default::default()
            },
            NetAdapter {
                descr: "Intel Ethernet Controller".to_string(),
                ..Default::default()
            },
        ];
        // PDH lowercases: exact case-insensitive match wins.
        assert_eq!(
            match_adapter("mediatek wi-fi 6e mt7902 wireless lan card", &adapters),
            Some((0, true))
        );
        assert_eq!(match_adapter("no such adapter", &adapters), None);
        assert!(match_adapter("x", &[]).is_none());
    }

    #[test]
    fn human_rates() {
        assert_eq!(human_bps(512.0), "512B/s");
        assert_eq!(human_bps(12_300.0), "12.3KB/s");
        assert_eq!(human_bps(2_500_000.0), "2.5MB/s");
    }

    fn tp(instance: &str, rx: Option<f64>, tx: Option<f64>) -> Throughput {
        Throughput {
            instance: instance.to_string(),
            rx_bps: rx,
            tx_bps: tx,
        }
    }

    fn adapter(descr: &str, class: AdapterClass) -> NetAdapter {
        NetAdapter {
            descr: descr.to_string(),
            class: class.as_str().to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn totals_unavailable_when_all_counters_missing() {
        // Every rate counter missing: totals must be None, never 0.0.
        let throughput = vec![tp("Ethernet", None, None), tp("WiFi", None, None)];
        let adapters = vec![
            adapter("Intel Ethernet Controller", AdapterClass::Ethernet),
            adapter("MediaTek Wi-Fi 6E", AdapterClass::Wifi),
        ];
        let (rx, tx, usable, total) = sum_non_loopback(&throughput, &adapters);
        assert_eq!(rx, None);
        assert_eq!(tx, None);
        // Two non-loopback instances were considered, zero produced a value.
        assert_eq!((usable, total), (0, 2));
        // And the wire format reports absence with a reason.
        let sample = NetSample {
            throughput,
            adapters,
            total_rx_bps_opt: rx,
            total_tx_bps_opt: tx,
            ..Default::default()
        };
        let stamp = ClockStamp {
            wall_millis: 1,
            mono_millis: 2,
        };
        let json = NetCollector::format_json(&sample, stamp, 0, 0);
        assert!(json.contains("\"total_rx_bps\":{\"v\":null,\"p\":\"unavailable\""));
        assert!(json.contains("no interface counters available"));
        assert!(!json.contains("\"total_rx_bps\":{\"v\":0"));
    }

    #[test]
    fn totals_measured_zero_stays_zero() {
        // A genuine measured zero across available counters stays a 0.0 sum.
        let throughput = vec![tp("Ethernet", Some(0.0), Some(0.0))];
        let adapters = vec![adapter("Intel Ethernet Controller", AdapterClass::Ethernet)];
        let (rx, tx, usable, _) = sum_non_loopback(&throughput, &adapters);
        assert_eq!(rx, Some(0.0));
        assert_eq!(tx, Some(0.0));
        assert_eq!(usable, 1);
    }

    #[test]
    fn totals_partial_sum_skips_missing_and_loopback() {
        // Mixed: one usable counter, one missing, plus loopback ignored.
        let throughput = vec![
            tp("Ethernet", Some(1000.0), Some(500.0)),
            tp("WiFi", None, None),
            tp("Loopback Pseudo-Interface 1", Some(9999.0), Some(9999.0)),
        ];
        let adapters = vec![
            adapter("Intel Ethernet Controller", AdapterClass::Ethernet),
            adapter("MediaTek Wi-Fi 6E", AdapterClass::Wifi),
            adapter("Software Loopback Interface 1", AdapterClass::Loopback),
        ];
        let (rx, tx, usable, total) = sum_non_loopback(&throughput, &adapters);
        assert_eq!(rx, Some(1000.0));
        assert_eq!(tx, Some(500.0));
        // Loopback excluded; only the one Ethernet counter was usable.
        assert_eq!((usable, total), (1, 3));
    }

    #[test]
    fn virtual_noise_filter() {
        assert!(is_virtual_noise(
            "WiFi-WFP Native MAC Layer LightWeight Filter-0000",
            "x"
        ));
        assert!(is_virtual_noise("Teredo Tunneling Pseudo-Interface", "x"));
        assert!(is_virtual_noise("6to4 Adapter", "x"));
        assert!(is_virtual_noise(
            "Local Area Connection* 8-QoS Packet Scheduler-0000",
            "x"
        ));
        assert!(!is_virtual_noise("WiFi", "MediaTek Wi-Fi 6E"));
        assert!(!is_virtual_noise(
            "Bluetooth Network Connection",
            "Bluetooth Device"
        ));
        assert!(!is_virtual_noise("Ethernet", "Intel Controller"));
    }

    /// Forced topology rebuild failure must retain the old counter table but
    /// mark it stale with the reason; a later successful rebuild installs the
    /// new topology and clears staleness. Exercised through the narrow
    /// `apply_rebuild` seam so no real PDH failure is required.
    #[cfg(windows)]
    #[test]
    fn topology_rebuild_failure_is_stale_then_success_recovers() {
        let mut c = NetCollector::new();
        c.tp = vec![("OLD".to_string(), 1, 2)];
        c.topology_stale = false;
        c.topology_error = None;
        // Forced rebuild failure: old usable counters retained, stale + reason.
        c.apply_rebuild(Err(
            "no Network Interface counters could be added".to_string()
        ));
        assert!(c.topology_stale);
        assert_eq!(
            c.topology_error.as_deref(),
            Some("no Network Interface counters could be added")
        );
        assert_eq!(c.tp.len(), 1, "old counter table must be retained");
        assert_eq!(c.tp[0].0, "OLD");
        // The sample a stale collector would emit carries the stale marker,
        // so retained old values never get fresh-topology semantics.
        let stale_sample = NetSample {
            topology_stale: c.topology_stale,
            topology_error: c.topology_error.clone(),
            ..Default::default()
        };
        let json = NetCollector::format_json(
            &stale_sample,
            ClockStamp {
                wall_millis: 1,
                mono_millis: 1,
            },
            0,
            0,
        );
        assert!(json.contains("\"topology_stale\":true"), "{json}");
        assert!(json.contains("counters could be added"), "{json}");
        // Later successful rebuild: new topology installed, staleness cleared.
        c.apply_rebuild(Ok(vec![("NEW".to_string(), 3, 4)]));
        assert!(!c.topology_stale);
        assert!(c.topology_error.is_none());
        assert_eq!(c.tp.len(), 1);
        assert_eq!(c.tp[0].0, "NEW");
    }
}
