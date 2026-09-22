//! GPU collector: adapter inventory (DXGI), per-engine utilization (PDH
//! GPU Engine), per-process video memory (PDH GPU Process Memory), and
//! derived adapter activity / discrete-GPU-awake detection.
//!
//! No admin required. GPU power, clocks, temperature, and PCIe state need
//! vendor APIs (NVML/ADL) and are reported Unavailable, never synthesized.
//!
//! LUID lesson learned live: the second LUID on the test machine is the
//! Microsoft Basic Render Driver (software), NOT a discrete GPU. Adapter
//! identity always comes from DXGI, never from LUID count.

use crate::collector::{Collector, CollectorError};
#[cfg(windows)]
use crate::cpu::InitRetry;
use crate::pdh::PdhQuery;
use pf_core::telemetry::{Clock, ClockStamp, FieldMeta, Provenance, escape_json};
use std::collections::HashMap;

// ---------------------------------------------------------------- DXGI ---

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct DxgiGuid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

/// IID_IDXGIFactory {7b7166ec-21c7-44ae-b21a-c9ae321ae369}
#[cfg(windows)]
const IID_IDXGIFACTORY: DxgiGuid = DxgiGuid {
    d1: 0x7b7166ec,
    d2: 0x21c7,
    d3: 0x44ae,
    d4: [0xb2, 0x1a, 0xc9, 0xae, 0x32, 0x1a, 0xe3, 0x69],
};

/// DXGI_ADAPTER_DESC. Must be 304 bytes (asserted in tests).
#[cfg(windows)]
#[repr(C)]
struct AdapterDesc {
    description: [u16; 128],
    vendor_id: u32,
    device_id: u32,
    subsys_id: u32,
    revision: u32,
    dedicated_video: u64,
    dedicated_system: u64,
    shared_system: u64,
    luid_low: u32,
    luid_high: i32,
}

#[cfg(windows)]
#[link(name = "dxgi")]
unsafe extern "system" {
    fn CreateDXGIFactory(riid: *const DxgiGuid, factory: *mut usize) -> i32;
}

/// Vtable slot indices (verified live against dxgi.dll).
#[cfg(windows)]
const VT_RELEASE: usize = 2;
#[cfg(windows)]
const VT_ENUM_ADAPTERS: usize = 7;
#[cfg(windows)]
const VT_GET_DESC: usize = 8;

/// Read a vtable slot from a COM object pointer.
#[cfg(windows)]
unsafe fn vslot(obj: usize, index: usize) -> usize {
    // Inner block required by edition-2024 unsafe-op-in-unsafe-fn rules.
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

#[derive(Debug, Clone, Default)]
pub struct GpuAdapter {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub dedicated_mb: u64,
    pub luid_low: u32,
    pub luid_high: u32,
}

pub fn vendor_name(vendor_id: u32) -> &'static str {
    match vendor_id {
        0x10DE => "NVIDIA",
        0x1002 => "AMD",
        0x8086 => "Intel",
        0x1414 => "Microsoft",
        _ => "unknown",
    }
}

/// Discrete-GPU heuristic with raw inputs kept alongside the verdict:
/// NVIDIA always discrete; Basic Render never; otherwise dedicated VRAM
/// over 1 GiB (AMD APUs report ~512MB, dGPUs report gigabytes).
pub fn is_discrete_heuristic(vendor_id: u32, dedicated_mb: u64) -> bool {
    match vendor_id {
        0x10DE => true,
        0x1414 => false,
        0x8086 => false,
        _ => dedicated_mb > 1024,
    }
}

#[cfg(windows)]
fn decode_desc_name(raw: &[u16; 128]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    raw[..end]
        .iter()
        .map(|&c| char::from_u32(c as u32).unwrap_or('\u{FFFD}'))
        .collect()
}

#[cfg(windows)]
pub fn enumerate_adapters() -> Result<Vec<GpuAdapter>, String> {
    // SAFETY: raw COM against dxgi.dll; vtable indices verified live,
    // every object released, HRESULTs checked.
    unsafe {
        let mut factory: usize = 0;
        let rc = CreateDXGIFactory(&IID_IDXGIFACTORY, &mut factory);
        if rc != 0 || factory == 0 {
            return Err(format!("CreateDXGIFactory failed: {rc:#X}"));
        }
        let enum_adapters: unsafe extern "system" fn(usize, u32, *mut usize) -> i32 =
            std::mem::transmute(vslot(factory, VT_ENUM_ADAPTERS));
        let mut out = Vec::new();
        let mut i = 0u32;
        loop {
            let mut adapter: usize = 0;
            let rc = enum_adapters(factory, i, &mut adapter);
            if rc != 0 || adapter == 0 {
                break; // DXGI_ERROR_NOT_FOUND ends the list
            }
            let get_desc: unsafe extern "system" fn(usize, *mut AdapterDesc) -> i32 =
                std::mem::transmute(vslot(adapter, VT_GET_DESC));
            let mut desc = std::mem::zeroed::<AdapterDesc>();
            let rc = get_desc(adapter, &mut desc);
            com_release(adapter);
            if rc == 0 {
                out.push(GpuAdapter {
                    index: i,
                    name: decode_desc_name(&desc.description),
                    vendor_id: desc.vendor_id,
                    device_id: desc.device_id,
                    dedicated_mb: desc.dedicated_video / (1024 * 1024),
                    luid_low: desc.luid_low,
                    luid_high: desc.luid_high as u32,
                });
            }
            i += 1;
        }
        com_release(factory);
        Ok(out)
    }
}

#[cfg(not(windows))]
pub fn enumerate_adapters() -> Result<Vec<GpuAdapter>, String> {
    Err("Windows required".to_string())
}

// ------------------------------------------------- PDH instance parsing ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineClass {
    Render3D,
    Compute,
    VideoDecode,
    VideoCodec,
    Copy,
    Other,
}

impl EngineClass {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineClass::Render3D => "3d",
            EngineClass::Compute => "compute",
            EngineClass::VideoDecode => "video_decode",
            EngineClass::VideoCodec => "video_codec",
            EngineClass::Copy => "copy",
            EngineClass::Other => "other",
        }
    }
}

pub fn classify_engtype(engtype: &str) -> EngineClass {
    let s = engtype.to_lowercase();
    if s.contains("decode") || s.contains("jpeg") {
        EngineClass::VideoDecode
    } else if s.contains("codec") || s.contains("encode") {
        EngineClass::VideoCodec
    } else if s.contains("compute") {
        EngineClass::Compute
    } else if s.contains("copy") {
        EngineClass::Copy
    } else if s == "3d" || s.ends_with(" 3d") {
        EngineClass::Render3D
    } else {
        EngineClass::Other
    }
}

#[derive(Debug, Clone)]
pub struct EngineKey {
    pub pid: u32,
    pub luid_low: u32,
    pub luid_high: u32,
    pub class: EngineClass,
}

/// Parse `pid_1472_luid_0x00000000_0x0000d6eb_phys_0_eng_0_engtype_3d`.
/// The engtype is always last but may contain spaces, so everything from
/// the `engtype` marker on is the type. PDH LUID order is (High, Low),
/// verified against DXGI AdapterLuid (Low=0xc361 on the AMD iGPU).
pub fn parse_engine_instance(inst: &str) -> Option<EngineKey> {
    let p: Vec<&str> = inst.split('_').collect();
    if p.len() < 11 || p[0] != "pid" || p[2] != "luid" || p[5] != "phys" || p[7] != "eng" {
        return None;
    }
    let et = p.iter().position(|&x| x == "engtype")?;
    if et + 1 >= p.len() {
        return None;
    }
    Some(EngineKey {
        pid: p[1].parse().ok()?,
        luid_high: u32::from_str_radix(p[3].trim_start_matches("0x"), 16).ok()?,
        luid_low: u32::from_str_radix(p[4].trim_start_matches("0x"), 16).ok()?,
        class: classify_engtype(&p[et + 1..].join("_")),
    })
}

#[derive(Debug, Clone)]
pub struct MemKey {
    pub pid: u32,
    pub luid_low: u32,
    pub luid_high: u32,
}

/// Parse `pid_1020_luid_0x00000000_0x0000c361_phys_0`.
pub fn parse_mem_instance(inst: &str) -> Option<MemKey> {
    let p: Vec<&str> = inst.split('_').collect();
    if p.len() != 7 || p[0] != "pid" || p[2] != "luid" || p[5] != "phys" {
        return None;
    }
    Some(MemKey {
        pid: p[1].parse().ok()?,
        luid_high: u32::from_str_radix(p[3].trim_start_matches("0x"), 16).ok()?,
        luid_low: u32::from_str_radix(p[4].trim_start_matches("0x"), 16).ok()?,
    })
}

// ---------------------------------------------------------- aggregation ---

use crate::nvml::Nvml;
pub use pf_core::analysis::{ACTIVE_UTIL_PCT, PowerAdapter};

/// Phase E seam: vendor GPU power readers. Power reads land here when
/// validated; until then every implementation returns None.
pub trait GpuPowerReader {
    fn name(&self) -> &'static str;
    fn read_w(&self) -> Option<f64>;
}

/// NVML dynamic-load reader (seam only: never returns watts yet).
pub struct NvmlReader {
    inner: Option<Nvml>,
}

impl NvmlReader {
    pub fn new() -> Self {
        Self {
            inner: Nvml::load().ok(),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.is_some()
    }
}

impl Default for NvmlReader {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuPowerReader for NvmlReader {
    fn name(&self) -> &'static str {
        "NVML (NVIDIA)"
    }

    fn read_w(&self) -> Option<f64> {
        // Seam only: no validated power path yet, so no watts.
        // Touch the handle so the seam stays wired without reading.
        let _ = self.inner.is_some();
        None
    }
}

/// Power backends with the dynamic-load seam noted. Same 3 entries and
/// shape as `pf_core::analysis::available_adapters`; only the
/// limitations text is extended.
pub fn available_adapters() -> Vec<PowerAdapter> {
    let mut v = pf_core::analysis::available_adapters();
    for a in &mut v {
        if !a.limitations.contains("nvml.dll probed at runtime") {
            a.limitations
                .push_str("; nvml.dll probed at runtime; power reads land here when validated");
        }
    }
    v
}

#[derive(Debug, Clone, Default)]
pub struct AdapterStats {
    pub total_util: f64,
    pub util_3d: f64,
    pub util_compute: f64,
    pub util_decode: f64,
    pub util_codec: f64,
    pub util_copy: f64,
    pub util_other: f64,
    pub engines_active: usize,
    pub engines_seen: usize,
    pub mem_dedicated_mb: f64,
    pub mem_shared_mb: f64,
    pub top_pids: Vec<(u32, f64)>,
}

impl AdapterStats {
    fn add_engine(&mut self, class: EngineClass, util: f64) {
        self.total_util += util;
        self.engines_seen += 1;
        if util > ACTIVE_UTIL_PCT {
            self.engines_active += 1;
        }
        match class {
            EngineClass::Render3D => self.util_3d += util,
            EngineClass::Compute => self.util_compute += util,
            EngineClass::VideoDecode => self.util_decode += util,
            EngineClass::VideoCodec => self.util_codec += util,
            EngineClass::Copy => self.util_copy += util,
            EngineClass::Other => self.util_other += util,
        }
    }
}

/// Pure aggregation: engine utils + video-memory per (luid_low, luid_high).
/// Engine-% values sum across engines (they are per-engine duty, not a
/// time fraction — never presented as "GPU %").
pub fn aggregate(
    engines: &[(EngineKey, f64)],
    mems: &[(MemKey, Option<f64>, Option<f64>)],
) -> HashMap<(u32, u32), AdapterStats> {
    let mut map: HashMap<(u32, u32), AdapterStats> = HashMap::new();
    let mut pid_util: HashMap<((u32, u32), u32), f64> = HashMap::new();
    for (key, util) in engines {
        let st = map.entry((key.luid_low, key.luid_high)).or_default();
        st.add_engine(key.class, *util);
        *pid_util
            .entry(((key.luid_low, key.luid_high), key.pid))
            .or_insert(0.0) += *util;
    }
    for (key, ded, sh) in mems {
        let st = map.entry((key.luid_low, key.luid_high)).or_default();
        if let Some(v) = ded {
            st.mem_dedicated_mb += *v;
        }
        if let Some(v) = sh {
            st.mem_shared_mb += *v;
        }
    }
    let mut pids_by_adapter: HashMap<(u32, u32), Vec<(u32, f64)>> = HashMap::new();
    for (((low, high), pid), util) in pid_util {
        pids_by_adapter
            .entry((low, high))
            .or_default()
            .push((pid, util));
    }
    for (luid, mut pids) in pids_by_adapter {
        pids.sort_by(|a, b| b.1.total_cmp(&a.1));
        if let Some(st) = map.get_mut(&luid) {
            // Only PIDs with real activity in the window; zero-duty
            // bindings are noise for both dashboard and attribution.
            st.top_pids = pids
                .into_iter()
                .filter(|(_, u)| *u > 0.01)
                .take(5)
                .collect();
        }
    }
    map
}

#[derive(Debug, Clone)]
pub struct AwakeVerdict {
    pub active: bool,
    pub confidence: &'static str,
    pub evidence: String,
}

/// Activity in the window proves awake (HIGH). Silence proves nothing
/// about power state (LOW) — there is no dGPU power sensor here.
pub fn awake_verdict(
    total_util: f64,
    engines_active: usize,
    top_pid: Option<(u32, f64)>,
) -> AwakeVerdict {
    if total_util > ACTIVE_UTIL_PCT {
        let who = top_pid
            .map(|(pid, u)| format!("; top pid {pid} ({u:.1}%)"))
            .unwrap_or_default();
        AwakeVerdict {
            active: true,
            confidence: "high",
            evidence: format!(
                "{engines_active} engines over {ACTIVE_UTIL_PCT}% in window (total {total_util:.1}%){who}"
            ),
        }
    } else {
        AwakeVerdict {
            active: false,
            confidence: "low",
            evidence:
                "no engine activity in window; adapter may still be powered (no power sensor)"
                    .to_string(),
        }
    }
}

// ------------------------------------------------------------- collector ---

#[derive(Debug, Clone)]
pub struct AdapterView {
    pub adapter: GpuAdapter,
    pub discrete: bool,
    pub stats: AdapterStats,
    pub awake: AwakeVerdict,
}

#[derive(Debug, Clone, Default)]
pub struct GpuSample {
    pub adapters: Vec<AdapterView>,
    /// True when the last instance-list rebuild failed and the previous
    /// (possibly stale) counter set is still in use.
    pub stale: bool,
}

pub struct GpuCollector {
    #[cfg(windows)]
    pdh: Option<PdhQuery>,
    #[cfg(windows)]
    engines: Vec<(EngineKey, usize)>,
    #[cfg(windows)]
    mems: Vec<(MemKey, usize, usize)>,
    #[cfg(windows)]
    init_retry: InitRetry,
    #[cfg(windows)]
    fresh: bool,
    #[cfg(windows)]
    tick: u64,
    adapters: Vec<GpuAdapter>,
    stale: bool,
}

/// Re-enumerate GPU Engine instances every N samples so processes that
/// started after init still get attributed.
#[cfg(windows)]
const REBUILD_EVERY: u64 = 40;
/// Sanity cap; the test machine shows ~313 engine instances.
#[cfg(windows)]
const MAX_ENGINE_COUNTERS: usize = 2000;

impl GpuCollector {
    pub fn new() -> Self {
        GpuCollector {
            #[cfg(windows)]
            pdh: None,
            #[cfg(windows)]
            engines: Vec::new(),
            #[cfg(windows)]
            mems: Vec::new(),
            #[cfg(windows)]
            init_retry: InitRetry::new(),
            #[cfg(windows)]
            fresh: false,
            #[cfg(windows)]
            tick: 0,
            adapters: Vec::new(),
            stale: false,
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
        // Adapter inventory is best-effort: engine data without DXGI names
        // is still attributable by LUID.
        self.adapters = enumerate_adapters().unwrap_or_default();
        match self.build_counters() {
            Ok(()) => {
                self.init_retry.note_success();
                Ok(())
            }
            Err(e) => Err(self.init_retry.note_failure(self.tick, e)),
        }
    }

    /// Open a query, add engine + memory counters, warm up rate counters.
    #[cfg(windows)]
    fn build_counters(&mut self) -> Result<(), String> {
        let pdh = PdhQuery::open()?;
        let mut engines = Vec::new();
        if let Ok(insts) = PdhQuery::enum_instances("GPU Engine") {
            for inst in insts {
                if engines.len() >= MAX_ENGINE_COUNTERS {
                    break;
                }
                if let Some(key) = parse_engine_instance(&inst) {
                    let h = pdh.add(&format!("\\GPU Engine({inst})\\Utilization Percentage"));
                    if h != 0 {
                        engines.push((key, h));
                    }
                }
            }
        }
        let mut mems = Vec::new();
        if let Ok(insts) = PdhQuery::enum_instances("GPU Process Memory") {
            for inst in insts {
                if let Some(key) = parse_mem_instance(&inst) {
                    let hd = pdh.add(&format!("\\GPU Process Memory({inst})\\Dedicated Usage"));
                    let hs = pdh.add(&format!("\\GPU Process Memory({inst})\\Shared Usage"));
                    if hd != 0 || hs != 0 {
                        mems.push((key, hd, hs));
                    }
                }
            }
        }
        if engines.is_empty() && mems.is_empty() {
            return Err("no GPU Engine / GPU Process Memory counters could be added".to_string());
        }
        pdh.collect()?;
        std::thread::sleep(std::time::Duration::from_millis(150));
        pdh.collect()?;
        self.pdh = Some(pdh);
        self.engines = engines;
        self.mems = mems;
        self.fresh = true;
        Ok(())
    }

    pub fn read(&mut self) -> Result<GpuSample, String> {
        #[cfg(windows)]
        {
            // Advance before init so a failed attempt still counts down the
            // retry backoff.
            self.tick = self.tick.wrapping_add(1);
            self.ensure_init()?;
            if self.tick.is_multiple_of(REBUILD_EVERY) {
                // Build the replacement first: on failure keep the old set
                // and flag staleness instead of going dark.
                let mut probe = GpuCollector::new();
                probe.adapters = std::mem::take(&mut self.adapters);
                match probe.build_counters() {
                    Ok(()) => {
                        self.pdh = probe.pdh;
                        self.engines = probe.engines;
                        self.mems = probe.mems;
                        self.adapters = probe.adapters;
                        self.stale = false;
                    }
                    Err(e) => {
                        self.adapters = probe.adapters;
                        self.stale = true;
                        let _ = e;
                    }
                }
            }
            let pdh = self.pdh.as_ref().ok_or("PDH query not initialized")?;
            if self.fresh {
                self.fresh = false;
            } else {
                pdh.collect()?;
            }
            let mut eng_vals = Vec::with_capacity(self.engines.len());
            for (key, h) in &self.engines {
                if let Some(v) = pdh.read_double(*h) {
                    eng_vals.push((key.clone(), v));
                }
            }
            let mut mem_vals = Vec::with_capacity(self.mems.len());
            for (key, hd, hs) in &self.mems {
                let d = pdh.read_double(*hd).map(|b| b / (1024.0 * 1024.0));
                let s = pdh.read_double(*hs).map(|b| b / (1024.0 * 1024.0));
                if d.is_some() || s.is_some() {
                    mem_vals.push((key.clone(), d, s));
                }
            }
            Ok(self.assemble(&eng_vals, &mem_vals))
        }
        #[cfg(not(windows))]
        {
            Err("Windows required".to_string())
        }
    }

    #[cfg(windows)]
    fn assemble(
        &self,
        eng_vals: &[(EngineKey, f64)],
        mem_vals: &[(MemKey, Option<f64>, Option<f64>)],
    ) -> GpuSample {
        let agg = aggregate(eng_vals, mem_vals);
        let mut views = Vec::new();
        for a in &self.adapters {
            let stats = agg
                .get(&(a.luid_low, a.luid_high))
                .cloned()
                .unwrap_or_default();
            let top = stats.top_pids.first().copied();
            views.push(AdapterView {
                adapter: a.clone(),
                discrete: is_discrete_heuristic(a.vendor_id, a.dedicated_mb),
                awake: awake_verdict(stats.total_util, stats.engines_active, top),
                stats,
            });
        }
        // LUIDs with activity but no DXGI match (should not happen, but
        // never drop observed energy... activity).
        for ((low, high), stats) in &agg {
            if !self
                .adapters
                .iter()
                .any(|a| a.luid_low == *low && a.luid_high == *high)
            {
                let top = stats.top_pids.first().copied();
                views.push(AdapterView {
                    adapter: GpuAdapter {
                        name: format!("unknown-luid-{low:08x}"),
                        luid_low: *low,
                        luid_high: *high,
                        ..Default::default()
                    },
                    discrete: false,
                    awake: awake_verdict(stats.total_util, stats.engines_active, top),
                    stats: stats.clone(),
                });
            }
        }
        views.sort_by_key(|v| v.adapter.index);
        GpuSample {
            adapters: views,
            stale: self.stale,
        }
    }

    /// Render a previously-read sample (collect once, reuse for store +
    /// dashboard).
    pub fn format_json(
        sample: &GpuSample,
        stamp: ClockStamp,
        t_start_ms: u64,
        t_end_ms: u64,
    ) -> String {
        use pf_core::telemetry::{Provenance, UnavailKind, json_num};
        // Aggregates are always present (0.0 when idle is a real measurement
        // of no activity, not missing data) and Derived.
        let dj = |v: f64| json_num(Some(v), Provenance::Derived, UnavailKind::NotSampled, "");
        let mut adapters = String::new();
        for (i, v) in sample.adapters.iter().enumerate() {
            if i > 0 {
                adapters.push(',');
            }
            let a = &v.adapter;
            let s = &v.stats;
            let mut pids = String::new();
            for (j, (pid, u)) in s.top_pids.iter().enumerate() {
                if j > 0 {
                    pids.push(',');
                }
                pids.push_str(&format!("{{\"pid\":{pid},\"util_pct\":{}}}", dj(*u)));
            }
            adapters.push_str(&format!(
                "{{\"index\":{},\"name\":\"{}\",\"vendor\":\"{}\",\"vendor_id\":\"{:#X}\",\
                 \"device_id\":\"{:#X}\",\"dedicated_mb\":{},\
                 \"luid\":\"{:#X}/{:#X}\",\"discrete\":{},\
                 \"total_util_pct\":{},\"util_3d\":{},\"util_compute\":{},\
                 \"util_decode\":{},\"util_codec\":{},\"util_copy\":{},\"util_other\":{},\
                 \"engines_active\":{},\"engines_seen\":{},\
                 \"mem_dedicated_mb\":{},\"mem_shared_mb\":{},\
                 \"top_pids\":[{pids}],\
                 \"awake\":{{\"active\":{},\"confidence\":\"{}\",\"evidence\":\"{}\"}}}}",
                a.index,
                escape_json(&a.name),
                vendor_name(a.vendor_id),
                a.vendor_id,
                a.device_id,
                a.dedicated_mb,
                a.luid_low,
                a.luid_high,
                v.discrete,
                dj(s.total_util),
                dj(s.util_3d),
                dj(s.util_compute),
                dj(s.util_decode),
                dj(s.util_codec),
                dj(s.util_copy),
                dj(s.util_other),
                s.engines_active,
                s.engines_seen,
                dj(s.mem_dedicated_mb),
                dj(s.mem_shared_mb),
                v.awake.active,
                v.awake.confidence,
                escape_json(&v.awake.evidence),
            ));
        }
        format!(
            "{{\"collector\":\"gpu\",\"wall_ms\":{},\"mono_ms\":{},\
             \"t_start_ms\":{},\"t_end_ms\":{},\
             \"stale\":{},\"adapters\":[{adapters}]}}",
            stamp.wall_millis, stamp.mono_millis, t_start_ms, t_end_ms, sample.stale,
        )
    }
}

impl Default for GpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for GpuCollector {
    fn name(&self) -> &'static str {
        "gpu"
    }

    fn capabilities(&self) -> Vec<FieldMeta> {
        vec![
            FieldMeta {
                name: "gpu.adapter.inventory",
                unit: "adapters",
                source: "DXGI CreateDXGIFactory/EnumAdapters/GetDesc",
                provenance: Provenance::Measured,
                interval_ms: 0,
                requires_admin: false,
                notes: "Enumerated once at init: name, vendor/device, VRAM, LUID",
            },
            FieldMeta {
                name: "gpu.adapter.util_total_pct",
                unit: "engine-%",
                source: "PDH GPU Engine(*)\\Utilization Percentage, summed per LUID",
                provenance: Provenance::Derived,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Sum of per-engine duty, NOT a time fraction; can exceed 100",
            },
            FieldMeta {
                name: "gpu.adapter.util_by_class_pct",
                unit: "engine-%",
                source: "PDH engtype classification (3d/compute/decode/codec/copy/other)",
                provenance: Provenance::Derived,
                interval_ms: 1000,
                requires_admin: false,
                notes: "'video codec' is ambiguous by design (encode vs decode unknown)",
            },
            FieldMeta {
                name: "gpu.adapter.mem_mb",
                unit: "MB",
                source: "PDH GPU Process Memory(*)\\Dedicated/Shared Usage per LUID",
                provenance: Provenance::Derived,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Summed over processes; per-PID detail feeds the process collector",
            },
            FieldMeta {
                name: "gpu.adapter.top_pids",
                unit: "pid+% ",
                source: "PDH GPU Engine instance names aggregated per PID",
                provenance: Provenance::Derived,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Top 5 PIDs by engine-%. PID->name resolution is process-collector phase",
            },
            FieldMeta {
                name: "gpu.adapter.awake",
                unit: "bool+confidence",
                source: "engine activity in window vs 0.5% threshold",
                provenance: Provenance::Derived,
                interval_ms: 1000,
                requires_admin: false,
                notes: "Activity proves awake (high); silence proves nothing (low) — no power sensor",
            },
            FieldMeta {
                name: "gpu.power_w",
                unit: "W",
                source: "NVML (NVIDIA) / ADL (AMD dGPU) — planned",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "No generic Windows GPU power API; iGPU power not separately measurable",
            },
            FieldMeta {
                name: "gpu.clock_mhz",
                unit: "MHz",
                source: "vendor APIs — planned",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "",
            },
            FieldMeta {
                name: "gpu.temperature_c",
                unit: "C",
                source: "vendor APIs — planned",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "Deliberately not a hardware-monitor clone; temp only matters vs throttling",
            },
            FieldMeta {
                name: "gpu.pcie_state",
                unit: "state",
                source: "vendor APIs / DXGI — planned",
                provenance: Provenance::Unavailable,
                interval_ms: 0,
                requires_admin: false,
                notes: "dGPU PCIe link state is the other half of awake detection",
            },
        ]
    }

    fn sample_json(&mut self, clock: &Clock) -> Result<String, CollectorError> {
        let t0 = clock.stamp();
        let sample = self.read().map_err(|e| CollectorError::new("gpu", e))?;
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
    fn parses_engine_instances_with_spaces() {
        let k =
            parse_engine_instance("pid_1472_luid_0x00000000_0x0000d6eb_phys_0_eng_0_engtype_3d")
                .unwrap();
        assert_eq!(k.pid, 1472);
        assert_eq!((k.luid_low, k.luid_high), (0xd6eb, 0));
        assert_eq!(k.class, EngineClass::Render3D);

        // PDH (High, Low) order matches DXGI AdapterLuid (Low 0xc361).
        let k = parse_engine_instance(
            "pid_1020_luid_0x00000000_0x0000c361_phys_0_eng_1_engtype_high priority 3d",
        )
        .unwrap();
        assert_eq!((k.luid_low, k.luid_high), (0xc361, 0));
        assert_eq!(k.class, EngineClass::Render3D);

        assert!(parse_engine_instance("pid_1_luid_0x0_0x1_phys_0").is_none());
        assert!(parse_engine_instance("garbage").is_none());
        assert!(parse_engine_instance("pid_x_luid_0x0_0x1_phys_0_eng_0_engtype_3d").is_none());
        assert!(parse_engine_instance("pid_1_luid_zz_0x1_phys_0_eng_0_engtype_3d").is_none());
    }

    #[test]
    fn classifies_all_observed_engtypes() {
        let cases = [
            ("3d", EngineClass::Render3D),
            ("high priority 3d", EngineClass::Render3D),
            ("high priority compute", EngineClass::Compute),
            ("compute 0", EngineClass::Compute),
            ("compute 1", EngineClass::Compute),
            ("video jpeg", EngineClass::VideoDecode),
            ("video decode 1", EngineClass::VideoDecode),
            ("video codec 0", EngineClass::VideoCodec),
            ("copy", EngineClass::Copy),
            ("security 1", EngineClass::Other),
            ("timer 0", EngineClass::Other),
        ];
        for (raw, want) in cases {
            assert_eq!(classify_engtype(raw), want, "engtype {raw}");
        }
    }

    #[test]
    fn parses_mem_instances() {
        let m = parse_mem_instance("pid_1020_luid_0x00000000_0x0000c361_phys_0").unwrap();
        assert_eq!((m.pid, m.luid_low, m.luid_high), (1020, 0xc361, 0));
        assert!(parse_mem_instance("pid_1_luid_0x0_0x1_phys_0_eng_0_engtype_3d").is_none());
        assert!(parse_mem_instance("pid_1_luid_0x0_0x1").is_none());
    }

    #[test]
    fn aggregation_sums_classes_and_ranks_pids() {
        let engines = vec![
            (
                EngineKey {
                    pid: 100,
                    luid_low: 1,
                    luid_high: 0,
                    class: EngineClass::Render3D,
                },
                2.5,
            ),
            (
                EngineKey {
                    pid: 100,
                    luid_low: 1,
                    luid_high: 0,
                    class: EngineClass::Copy,
                },
                0.2,
            ),
            (
                EngineKey {
                    pid: 200,
                    luid_low: 1,
                    luid_high: 0,
                    class: EngineClass::VideoDecode,
                },
                5.0,
            ),
            (
                EngineKey {
                    pid: 300,
                    luid_low: 2,
                    luid_high: 0,
                    class: EngineClass::Render3D,
                },
                0.1,
            ),
        ];
        let mems = vec![(
            MemKey {
                pid: 100,
                luid_low: 1,
                luid_high: 0,
            },
            Some(300.0),
            Some(50.0),
        )];
        let agg = aggregate(&engines, &mems);
        let a = &agg[&(1, 0)];
        assert!((a.total_util - 7.7).abs() < 1e-9);
        assert!((a.util_3d - 2.5).abs() < 1e-9);
        assert_eq!(a.engines_active, 2); // 2.5 and 5.0 over threshold
        assert_eq!(a.engines_seen, 3);
        assert_eq!(a.top_pids[0].0, 200);
        assert!((a.mem_dedicated_mb - 300.0).abs() < 1e-9);
        assert!((agg[&(2, 0)].total_util - 0.1).abs() < 1e-9);
    }

    #[test]
    fn awake_verdict_is_asymmetric_by_design() {
        let on = awake_verdict(3.0, 2, Some((1234, 2.5)));
        assert!(on.active);
        assert_eq!(on.confidence, "high");
        assert!(on.evidence.contains("1234"));
        let edge = awake_verdict(0.5, 0, None);
        assert!(!edge.active);
        let off = awake_verdict(0.0, 0, None);
        assert!(!off.active);
        assert_eq!(off.confidence, "low");
    }

    #[test]
    fn discrete_heuristic() {
        assert!(is_discrete_heuristic(0x10DE, 0));
        assert!(!is_discrete_heuristic(0x1414, 99999));
        assert!(!is_discrete_heuristic(0x8086, 2048));
        assert!(!is_discrete_heuristic(0x1002, 495)); // AMD APU
        assert!(is_discrete_heuristic(0x1002, 8192)); // AMD dGPU
        assert_eq!(vendor_name(0x1002), "AMD");
        assert_eq!(vendor_name(0xFFFF), "unknown");
    }

    #[test]
    fn power_adapters_report_unavailability_without_fake_watts() {
        let adapters = available_adapters();
        assert_eq!(adapters.len(), 3);
        for a in &adapters {
            // Nothing linked on this build: unavailable with a reason.
            assert!(!a.available, "{}", a.name);
            assert!(!a.limitations.is_empty(), "{}", a.name);
            assert_eq!(a.units, "W");
            assert_eq!(a.provenance, "unavailable");
        }
        let names: Vec<&str> = adapters.iter().map(|a| a.name.as_str()).collect();
        assert!(names.iter().any(|n| n.contains("NVML")));
        assert!(names.iter().any(|n| n.contains("ADL")));
        assert!(names.iter().any(|n| n.contains("Intel")));
    }

    #[cfg(windows)]
    #[test]
    fn adapter_desc_layout_is_304_bytes() {
        assert_eq!(std::mem::size_of::<AdapterDesc>(), 304);
    }
}
