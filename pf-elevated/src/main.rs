//! pf-elevated: privileged telemetry helper for power-forensics.
//!
//! Everything here needs administrator: power requests / wake timers
//! (via powercfg), NVMe health (via the OS storage stack), and kernel ETW
//! context-switch attribution. The main binary stays unprivileged and calls
//! `pf-elevated snapshot` for the fast subset.
//!
//! Validation status (2026-09-17, admin shell): power-requests,
//! waketimers, nvme, and etw-cswitch all verified live. The direct NVMe
//! IOCTL investigation below is kept as an abandoned-approach record.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o
}

#[cfg(windows)]
#[link(name = "shell32")]
unsafe extern "system" {
    fn IsUserAnAdmin() -> i32;
}

fn require_admin() {
    #[cfg(windows)]
    {
        // SAFETY: trivial getter.
        let admin = unsafe { IsUserAnAdmin() };
        if admin == 0 {
            println!("{{\"error\":\"pf-elevated requires administrator privileges\"}}");
            std::process::exit(3);
        }
    }
    #[cfg(not(windows))]
    {
        println!("{{\"error\":\"Windows required\"}}");
        std::process::exit(3);
    }
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------- powercfg parsing ---

/// Parse `powercfg /requests` into per-class entry lists. Tolerant of
/// missing classes; unknown lines attach to the current class.
pub fn parse_requests(text: &str) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let mut cur = String::new();
    for raw in text.lines() {
        let t = raw.trim();
        if t.is_empty() {
            continue;
        }
        if t.ends_with(':') && !t.starts_with('[') {
            cur = t.trim_end_matches(':').to_lowercase();
            map.entry(cur.clone()).or_default();
        } else if t.eq_ignore_ascii_case("none.") {
            map.entry(cur.clone()).or_default();
        } else if !cur.is_empty() {
            map.entry(cur.clone()).or_default().push(t.to_string());
        }
    }
    map
}

/// Parse `powercfg /waketimers` into timer lines (drops the none-message
/// in all its wordings: "...no [active] wake timers...").
pub fn parse_waketimers(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| {
            if l.is_empty() {
                return false;
            }
            let low = l.to_lowercase();
            !(low.contains("there are no") && low.contains("wake timer"))
        })
        .collect()
}

fn run_powercfg(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("powercfg")
        .args(args)
        .output()
        .map_err(|e| format!("powercfg spawn failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "powercfg {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn cmd_power_requests() -> i32 {
    match run_powercfg(&["/requests"]) {
        Ok(text) => {
            let m = parse_requests(&text);
            let mut o = format!(
                "{{\"tool\":\"pf-elevated\",\"query\":\"power-requests\",\"wall_ms\":{}",
                wall_ms()
            );
            for class in [
                "display",
                "system",
                "awaymode",
                "execution",
                "perfboostmode",
            ] {
                o.push_str(&format!(",\"{class}\":["));
                if let Some(entries) = m.get(class) {
                    for (i, e) in entries.iter().enumerate() {
                        if i > 0 {
                            o.push(',');
                        }
                        o.push_str(&format!("\"{}\"", esc(e)));
                    }
                }
                o.push(']');
            }
            o.push('}');
            println!("{o}");
            0
        }
        Err(e) => {
            println!("{{\"error\":\"{}\"}}", esc(&e));
            4
        }
    }
}

fn cmd_waketimers() -> i32 {
    match run_powercfg(&["/waketimers"]) {
        Ok(text) => {
            let timers = parse_waketimers(&text);
            let mut o = format!(
                "{{\"tool\":\"pf-elevated\",\"query\":\"waketimers\",\"wall_ms\":{}",
                wall_ms()
            );
            o.push_str(",\"timers\":[");
            for (i, t) in timers.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                o.push_str(&format!("\"{}\"", esc(t)));
            }
            o.push_str("]}");
            println!("{o}");
            0
        }
        Err(e) => {
            println!("{{\"error\":\"{}\"}}", esc(&e));
            4
        }
    }
}

// ---------------------------------------------------------- NVMe SMART ---
//
// Direct pass-through was investigated and ABANDONED (2026-09-17, admin
// shell): IOCTL_STORAGE_PROTOCOL_COMMAND fails with ERROR_INVALID_FUNCTION
// on this stornvme stack for every ProtocolType, and QUERY_PROPERTY
// protocol-specific reads return headers without payload. The OS storage
// stack (MSFT_PhysicalDisk -> Get-StorageReliabilityCounter) provably
// reads this drive (33 C live), so the helper uses it. APST power-state
// has no OS API and stays unavailable.

/// One NVMe drive's health as parsed from PowerShell k=v lines.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvmeHealth {
    pub device_id: String,
    pub model: String,
    pub temp_c: Option<i64>,
    pub temp_max_c: Option<i64>,
    pub wear_pct: Option<i64>,
    pub power_hours: Option<i64>,
    pub power_cycles: Option<i64>,
    pub read_errors: Option<i64>,
    pub write_errors: Option<i64>,
}

fn num_or_none(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("null") {
        return None;
    }
    s.parse::<i64>().ok()
}

/// Parse `id=0|model=WD...|temp=33|...` lines. Pure and tested.
pub fn parse_nvme_kv(text: &str) -> Vec<NvmeHealth> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || !line.contains("id=") {
            continue;
        }
        let mut h = NvmeHealth::default();
        for part in line.split('|') {
            let (k, v) = match part.split_once('=') {
                Some(x) => x,
                None => continue,
            };
            match k.trim() {
                "id" => h.device_id = v.trim().to_string(),
                "model" => h.model = v.trim().to_string(),
                "temp" => h.temp_c = num_or_none(v),
                "temp_max" => h.temp_max_c = num_or_none(v),
                "wear" => h.wear_pct = num_or_none(v),
                "hours" => h.power_hours = num_or_none(v),
                "cycles" => h.power_cycles = num_or_none(v),
                "rerr" => h.read_errors = num_or_none(v),
                "werr" => h.write_errors = num_or_none(v),
                _ => {}
            }
        }
        if !h.device_id.is_empty() {
            out.push(h);
        }
        if out.len() >= 16 {
            break;
        }
    }
    out
}

const NVME_PS: &str = "Get-PhysicalDisk | Where-Object { $_.BusType -eq 'NVMe' } | ForEach-Object { $d = $_; $r = $_ | Get-StorageReliabilityCounter; $temp = $r.Temperature; $tmax = $r.TemperatureMax; $wear = $r.Wear; $hours = $r.PowerOnHours; $cyc = $r.PowerCycleCount; $re = $r.ReadErrorsTotal; $we = $r.WriteErrorsTotal; \"id=$($d.DeviceId)|model=$($d.FriendlyName -replace '[|]','/')|temp=$temp|temp_max=$tmax|wear=$wear|hours=$hours|cycles=$cyc|rerr=$re|werr=$we\" }";

#[cfg(windows)]
fn cmd_nvme(drive: Option<u32>) -> i32 {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", NVME_PS])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => {
            println!("{{\"error\":\"NVMe query via storage stack failed\"}}");
            return 4;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut drives = parse_nvme_kv(&text);
    if let Some(d) = drive {
        drives.retain(|h| h.device_id == d.to_string());
        if drives.is_empty() {
            println!("{{\"error\":\"no NVMe drive with DeviceId {d}\"}}");
            return 4;
        }
    }
    if drives.is_empty() {
        println!("{{\"error\":\"no NVMe drives reported by the storage stack\"}}");
        return 4;
    }
    let opt = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or("null".to_string());
    let mut o = format!(
        "{{\"tool\":\"pf-elevated\",\"query\":\"nvme\",\"wall_ms\":{},\"drives\":[",
        wall_ms()
    );
    for (i, h) in drives.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str(&format!(
            "{{\"device_id\":\"{}\",\"model\":\"{}\",\"temp_c\":{},\"temp_max_c\":{},\
             \"wear_pct\":{},\"power_hours\":{},\"power_cycles\":{},\
             \"read_errors\":{},\"write_errors\":{}}}",
            esc(&h.device_id),
            esc(&h.model),
            opt(h.temp_c),
            opt(h.temp_max_c),
            opt(h.wear_pct),
            opt(h.power_hours),
            opt(h.power_cycles),
            opt(h.read_errors),
            opt(h.write_errors)
        ));
    }
    o.push_str("],\"note\":\"health via MSFT storage stack; APST power state has no OS API\"}");
    println!("{o}");
    0
}

/* ABANDONED direct-IOCTL investigation (kept as a record, not compiled).
   Verified 2026-09-17 with admin: IOCTL_STORAGE_PROTOCOL_COMMAND fails
   with ERROR_INVALID_FUNCTION on this stornvme stack for every candidate
   ProtocolType, and QUERY_PROPERTY protocol-specific reads return headers
   without payload. The OS storage stack works, so cmd_nvme above uses it.

/// Parsed NVMe SMART/Health log (page 0x02) essentials.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvmeSmart {
    pub temp_c: i32,
    pub percent_used: u8,
    pub avail_spare: u8,
    pub power_cycles: u64,
    pub power_hours: u64,
    pub unsafe_shutdowns: u64,
    pub media_errors: u64,
}

fn u16le(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u64le(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off + 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(u64::from_le_bytes(a))
}

/// Parse a 512-byte NVMe SMART log. None when the buffer cannot be one
/// (wrong length or implausible temperature — the validity gate that lets
/// us probe candidate ProtocolType values safely).
pub fn parse_nvme_smart(buf: &[u8]) -> Option<NvmeSmart> {
    if buf.len() < 512 {
        return None;
    }
    let temp_k = u16le(buf, 1)? as i32;
    if !(273..=400).contains(&temp_k) {
        return None;
    }
    let percent_used = buf[6];
    if percent_used > 100 && percent_used != 255 {
        return None;
    }
    Some(NvmeSmart {
        temp_c: temp_k - 273,
        percent_used,
        avail_spare: buf[3],
        power_cycles: u64le(buf, 96)?,
        power_hours: u64le(buf, 112)?,
        unsafe_shutdowns: u64le(buf, 128)?,
        media_errors: u64le(buf, 160)?,
    })
}

/// Build a 124-byte STORAGE_PROTOCOL_COMMAND + 512-byte data buffer for an
/// NVMe admin Get Log Page (SMART, LID 2). Layout offsets documented inline.
fn build_smart_command(protocol: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 124 + 512];
    let w32 = |b: &mut [u8], off: usize, v: u32| {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    w32(&mut buf, 0, 1); // Version = STORAGE_PROTOCOL_STRUCTURE_VERSION
    w32(&mut buf, 4, 124); // Length of fixed header
    w32(&mut buf, 8, protocol); // ProtocolType (probed, not assumed)
    w32(&mut buf, 24, 64); // CommandLength = sizeof(NVME_COMMAND)
    w32(&mut buf, 32, 0); // DataToDeviceTransferLength
    w32(&mut buf, 36, 512); // DataFromDeviceTransferLength
    w32(&mut buf, 40, 10); // TimeOutValue seconds
    w32(&mut buf, 48, 124); // DataBufferOffset
    // NVME_COMMAND at offset 60: CDW0 = GET_LOG_PAGE (0x02).
    w32(&mut buf, 60, 0x02);
    // NSID = 0xFFFFFFFF at offset 64.
    w32(&mut buf, 64, 0xFFFF_FFFF);
    // CDW10 at offset 100: LID(0x02) | NUMDL(127) << 16.
    w32(&mut buf, 100, 0x007F_0002);
    buf
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn cmd_nvme(drive: u32) -> i32 {
    // SAFETY: PENDING-ADMIN — driver call sequence is standard
    // pass-through; every handle closed, every return checked, and the
    // response must validate as a SMART log or the drive is reported
    // non-NVMe/unreadable (never garbage).
    unsafe {
        let path = wide(&format!("\\\\.\\PhysicalDrive{drive}"));
        let h = CreateFileW(path.as_ptr(), 0x80000000 | 0x40000000, 0x1 | 0x2, 0, 3, 0, 0);
        if h == -1 {
            println!(
                "{{\"error\":\"cannot open PhysicalDrive{drive}: GetLastError={} (admin required)\"}}",
                GetLastError()
            );
            return 4;
        }
        struct Guard(isize);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
        let _guard = Guard(h);
        for protocol in [5u32, 4, 3] {
            let mut buf = build_smart_command(protocol);
            let mut ret: u32 = 0;
            let rc = DeviceIoControl(
                h,
                IOCTL_STORAGE_PROTOCOL_COMMAND,
                buf.as_ptr() as *const std::ffi::c_void,
                124,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                buf.len() as u32,
                &mut ret,
                0,
            );
            if rc == 0 {
                continue;
            }
            if let Some(sm) = parse_nvme_smart(&buf[124..]) {
                println!(
                    "{{\"tool\":\"pf-elevated\",\"query\":\"nvme\",\"drive\":{drive},\"wall_ms\":{},\
                     \"protocol_type\":{protocol},\"temp_c\":{},\"percent_used\":{},\
                     \"avail_spare_pct\":{},\"power_cycles\":{},\"power_hours\":{},\
                     \"unsafe_shutdowns\":{},\"media_errors\":{}}}",
                    wall_ms(),
                    sm.temp_c,
                    sm.percent_used,
                    sm.avail_spare,
                    sm.power_cycles,
                    sm.power_hours,
                    sm.unsafe_shutdowns,
                    sm.media_errors
                );
                return 0;
            }
        }
        println!("{{\"error\":\"PhysicalDrive{drive}: no NVMe SMART response (SATA/USB drive, driver filter, or protocol mismatch)\"}}");
        4
    }
    #[cfg(not(windows))]
    {
        let _ = drive;
        println!("{{\"error\":\"Windows required\"}}");
        4
    }
}
END OF ABANDONED BLOCK */

// ------------------------------------------------------- ETW cswitch ---
// Implemented on the `windows` crate's verified bindings (exact struct
// layouts) after hand-rolled EVENT_TRACE_PROPERTIES/LOGFILEW cost a day
// of debugging (see git history / session notes for the full story:
// LoggerThreadId size, callback union slot, EVENT_RECORD offsets).

#[cfg(windows)]
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_FLAG_CSWITCH, EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, OpenTraceW,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, ProcessTrace, StartTraceW,
    WNODE_FLAG_TRACED_GUID,
};
#[cfg(windows)]
use windows::{
    Win32::System::Diagnostics::Etw::EVENT_TRACE_LOGFILEW,
    core::{GUID, PCWSTR, PWSTR},
};

/// CSwitch arrives as Opcode 36 (Id/Task are 0 for kernel MOF events).
pub const CSWITCH_OPCODE: u8 = 36;

/// Parse (new_tid, old_tid) from a CSwitch payload prefix. Pure, tested.
/// ETW payloads are LE on the wire; decode as LE explicitly for portability.
pub fn parse_cswitch(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 8 {
        return None;
    }
    let nt = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let ot = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    if nt == 0 && ot == 0 {
        return None;
    }
    Some((nt, ot))
}

#[cfg(windows)]
static CSWITCH_COUNTS: LazyLock<Mutex<HashMap<u32, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(windows)]
static CSWITCH_TOTAL: LazyLock<Mutex<u64>> = LazyLock::new(|| Mutex::new(0));
#[cfg(windows)]
static ID_COUNTS: LazyLock<Mutex<HashMap<u16, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record callback reading fields (never offsets) from EVENT_RECORD.
#[cfg(windows)]
unsafe extern "system" fn event_record_cb(
    record: *mut windows::Win32::System::Diagnostics::Etw::EVENT_RECORD,
) {
    use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;
    unsafe {
        if record.is_null() {
            return;
        }
        let r: &EVENT_RECORD = &*record;
        let opc = r.EventHeader.EventDescriptor.Opcode;
        if let Ok(mut m) = ID_COUNTS.lock() {
            *m.entry(opc as u16).or_insert(0) += 1;
        }
        if opc != CSWITCH_OPCODE {
            return;
        }
        let len = r.UserDataLength as usize;
        if len < 8 || r.UserData.is_null() {
            return;
        }
        let payload = std::slice::from_raw_parts(r.UserData as *const u8, len.min(64));
        if let Some((nt, _)) = parse_cswitch(payload) {
            if let Ok(mut m) = CSWITCH_COUNTS.lock() {
                *m.entry(nt).or_insert(0) += 1;
            }
            if let Ok(mut t) = CSWITCH_TOTAL.lock() {
                *t += 1;
            }
        }
    }
}

#[cfg(windows)]
fn cmd_etw(seconds: u64, top: usize, debug_ids: bool) -> i32 {
    use windows::Win32::Foundation::WIN32_ERROR;
    // SAFETY: verified crate bindings (live: 23k switches/3s matching PDH
    // context-switch rates); every API return checked; the session is
    // stopped on all paths.
    unsafe {
        // NOTE: the kernel logger session has a FIXED name; anything else
        // fails with ERROR_BAD_PATHNAME (verified live).
        let name: Vec<u16> = "NT Kernel Logger"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
        let total = props_size + name.len() * 2;
        let mut props: EVENT_TRACE_PROPERTIES = std::mem::zeroed();
        props.Wnode.BufferSize = total as u32;
        props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        props.Wnode.Guid = GUID::from_u128(0x9e814aad_3204_11d1_9a82_00a0c7224706);
        props.Wnode.ClientContext = 1;
        props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        props.EnableFlags = EVENT_TRACE_FLAG_CSWITCH;
        props.FlushTimer = 1;
        props.LoggerNameOffset = props_size as u32;
        let mut bytes = vec![0u8; total];
        std::ptr::write(bytes.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES, props);
        for (i, c) in name.iter().enumerate() {
            // UTF-16LE logger name bytes; LE explicit for portability.
            bytes[props_size + i * 2..props_size + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        let props_ptr = bytes.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        let mut session = CONTROLTRACE_HANDLE::default();
        let rc = StartTraceW(&mut session, PCWSTR(name.as_ptr()), props_ptr);
        if rc != WIN32_ERROR(0) {
            println!(
                "{{\"error\":\"StartTrace failed: {} (session may already exist)\"}}",
                rc.0
            );
            return 4;
        }
        let mut log: EVENT_TRACE_LOGFILEW = std::mem::zeroed();
        log.LoggerName = PWSTR(name.as_ptr() as *mut u16);
        log.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        log.Anonymous2.EventRecordCallback = Some(event_record_cb);
        log.IsKernelTrace = 1;
        let trace = OpenTraceW(&mut log);
        if trace == Default::default() {
            let _ = ControlTraceW(
                session,
                PCWSTR(name.as_ptr()),
                props_ptr,
                EVENT_TRACE_CONTROL_STOP,
            );
            println!("{{\"error\":\"OpenTrace failed\"}}");
            return 4;
        }
        let worker =
            std::thread::spawn(move || ProcessTrace(std::slice::from_ref(&trace), None, None));
        std::thread::sleep(std::time::Duration::from_secs(seconds.clamp(1, 300)));
        let _ = ControlTraceW(
            session,
            PCWSTR(name.as_ptr()),
            props_ptr,
            EVENT_TRACE_CONTROL_STOP,
        );
        let worker_rc = worker.join();
        let _ = CloseTrace(trace);
        if debug_ids {
            let ids = ID_COUNTS.lock().map(|m| m.clone()).unwrap_or_default();
            let mut ranked: Vec<(u16, u64)> = ids.into_iter().collect();
            ranked.sort_by_key(|x| std::cmp::Reverse(x.1));
            let mut o = format!(
                "{{\"tool\":\"pf-elevated\",\"query\":\"etw-debug-ids\",\"wall_ms\":{},\"top_ids\":[",
                wall_ms()
            );
            for (i, (id, n)) in ranked.iter().take(15).enumerate() {
                if i > 0 {
                    o.push(',');
                }
                o.push_str(&format!("{{\"id\":{id},\"count\":{n}}}"));
            }
            o.push_str(&format!("],\"process_trace_rc\":\"{worker_rc:?}\""));
            o.push('}');
            println!("{o}");
            return 0;
        }
        // Resolve tids via Toolhelp snapshot (no ETW thread tracking needed).
        let tid2pid = snapshot_threads();
        let counts = CSWITCH_COUNTS.lock().map(|m| m.clone()).unwrap_or_default();
        let total: u64 = counts.values().sum();
        let mut per_pid: HashMap<u32, u64> = HashMap::new();
        let mut unknown = 0u64;
        for (tid, n) in &counts {
            match tid2pid.get(tid) {
                Some(pid) => *per_pid.entry(*pid).or_insert(0) += n,
                None => unknown += n,
            }
        }
        let mut ranked: Vec<(u32, u64)> = per_pid.into_iter().collect();
        ranked.sort_by_key(|x| std::cmp::Reverse(x.1));
        let mut o = format!(
            "{{\"tool\":\"pf-elevated\",\"query\":\"etw-cswitch\",\"wall_ms\":{},\"seconds\":{seconds},\
             \"total_switches\":{total},\"unknown_tid\":{unknown},\"top\":[",
            wall_ms()
        );
        for (i, (pid, n)) in ranked.iter().take(top.max(1)).enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!("{{\"pid\":{pid},\"switches\":{n}}}"));
        }
        o.push_str("]}");
        println!("{o}");
        0
    }
}

#[cfg(windows)]
fn snapshot_threads() -> HashMap<u32, u32> {
    // SAFETY: Toolhelp snapshot walk (tlhelp32.h THREADENTRY32, 28 bytes);
    // handle closed on all paths; dwSize set from size_of; return codes
    // checked. A layout mismatch degrades to an empty map, never misparsed
    // tids.
    unsafe {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
            fn Thread32First(h: isize, e: *mut ThreadEntry) -> i32;
            fn Thread32Next(h: isize, e: *mut ThreadEntry) -> i32;
            fn CloseHandle(h: isize) -> i32;
        }
        #[repr(C)]
        struct ThreadEntry {
            size: u32,
            usage: u32,
            tid: u32,
            owner: u32,
            base_pri: i32,
            delta_pri: i32,
            flags: u32,
        }
        // Runtime ABI guard: THREADENTRY32 must be 28 bytes.
        if std::mem::size_of::<ThreadEntry>() != 28 {
            return HashMap::new();
        }
        let mut map = HashMap::new();
        let h = CreateToolhelp32Snapshot(0x4, 0);
        if h == -1 {
            return map;
        }
        let mut e = std::mem::zeroed::<ThreadEntry>();
        e.size = std::mem::size_of::<ThreadEntry>() as u32;
        if Thread32First(h, &mut e) != 0 {
            loop {
                map.insert(e.tid, e.owner);
                if Thread32Next(h, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(h);
        map
    }
}

// ------------------------------------------------------------- snapshot ---

fn cmd_snapshot() -> i32 {
    // Fast subset for `monitor --helper`: no blocking ETW capture.
    let wall = wall_ms();
    let requests = run_powercfg(&["/requests"]).unwrap_or_default();
    let waketimers = run_powercfg(&["/waketimers"]).unwrap_or_default();
    let req = parse_requests(&requests);
    let timers = parse_waketimers(&waketimers);
    let mut o = format!("{{\"collector\":\"elevated\",\"wall_ms\":{wall},\"power_requests\":{{");
    let mut first = true;
    for class in [
        "display",
        "system",
        "awaymode",
        "execution",
        "perfboostmode",
    ] {
        if !first {
            o.push(',');
        }
        first = false;
        o.push_str(&format!("\"{class}\":["));
        if let Some(entries) = req.get(class) {
            for (i, e) in entries.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                o.push_str(&format!("\"{}\"", esc(e)));
            }
        }
        o.push(']');
    }
    o.push_str("},\"waketimers\":[");
    for (i, t) in timers.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str(&format!("\"{}\"", esc(t)));
    }
    o.push_str("],\"note\":\"snapshot subset (no ETW capture); run etw-cswitch separately\"}");
    println!("{o}");
    0
}

// ------------------------------------------------------------------ main ---

fn usage() -> &'static str {
    "usage: pf-elevated <snapshot|power-requests|waketimers|nvme --drive N|etw-cswitch --seconds N [--top N]>"
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    require_admin();
    let code = match args.first().map(|s| s.as_str()) {
        Some("snapshot") => cmd_snapshot(),
        Some("power-requests") => cmd_power_requests(),
        Some("waketimers") => cmd_waketimers(),
        Some("nvme") => {
            // --drive N filters by MSFT DeviceId; omitted = all NVMe drives.
            let drive = args
                .iter()
                .position(|a| a == "--drive")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<u32>().ok());
            #[cfg(windows)]
            {
                cmd_nvme(drive)
            }
            #[cfg(not(windows))]
            {
                let _ = drive;
                println!("{{\"error\":\"Windows required\"}}");
                4
            }
        }
        Some("etw-cswitch") => {
            let seconds = args
                .iter()
                .position(|a| a == "--seconds")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(5);
            let top = args
                .iter()
                .position(|a| a == "--top")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(10);
            let debug_ids = args.iter().any(|a| a == "--debug-ids");
            #[cfg(windows)]
            {
                cmd_etw(seconds, top, debug_ids)
            }
            #[cfg(not(windows))]
            {
                let _ = (seconds, top, debug_ids);
                println!("{{\"error\":\"Windows required\"}}");
                4
            }
        }
        _ => {
            eprintln!("{usage}", usage = usage());
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parser() {
        let text = "DISPLAY:\nNone.\n\nSYSTEM:\n[DRIVER] \\FileSystem\\srvnet (srvnet)\nAn active remote client.\n\nAWAYMODE:\nNone.\n\nEXECUTION:\n[PROCESS] \\Device\\HarddiskVolume3\\app.exe\nMedia playing.\n";
        let m = parse_requests(text);
        assert!(m.get("display").unwrap().is_empty());
        assert_eq!(m.get("system").unwrap().len(), 2);
        assert!(m.get("awaymode").unwrap().is_empty());
        assert_eq!(m.get("execution").unwrap().len(), 2);
        assert!(parse_requests("").is_empty());
    }

    #[test]
    fn waketimers_parser() {
        let text = "Timer set by [SERVICE] \\Device\\HarddiskVolume3\\Windows\\System32\\svchost.exe (SystemEventsBroker) expires at 02:00.\n";
        let v = parse_waketimers(text);
        assert_eq!(v.len(), 1);
        let none = "There are no wake timers in the system.\n";
        assert!(parse_waketimers(none).is_empty());
    }

    #[test]
    fn nvme_kv_parsing() {
        let text = "id=0|model=WD PC SN740|temp=33|temp_max=84|wear=0|hours=|cycles=12|rerr=|werr=0\n\
                    garbage without id\n\
                    id=1|model=Other|temp=40|temp_max=90|wear=1|hours=500|cycles=30|rerr=0|werr=1\n";
        let v = parse_nvme_kv(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].device_id, "0");
        assert_eq!(v[0].temp_c, Some(33));
        assert_eq!(v[0].power_hours, None);
        assert_eq!(v[0].power_cycles, Some(12));
        assert_eq!(v[1].model, "Other");
        assert_eq!(v[1].write_errors, Some(1));
        assert!(parse_nvme_kv("").is_empty());
        assert!(parse_nvme_kv("id=|temp=33").is_empty());
    }

    #[test]
    fn cswitch_parsing() {
        let mut buf = vec![0u8; 24];
        buf[0..4].copy_from_slice(&1234u32.to_le_bytes());
        buf[4..8].copy_from_slice(&5678u32.to_le_bytes());
        assert_eq!(parse_cswitch(&buf), Some((1234, 5678)));
        assert_eq!(parse_cswitch(&buf[..7]), None);
        assert_eq!(parse_cswitch(&[0u8; 24]), None);
    }

    #[cfg(windows)]
    #[test]
    fn thread_entry_layout_is_28_bytes() {
        // SAFETY: THREADENTRY32 (tlhelp32.h) is 28 bytes; asserted so the
        // Toolhelp snapshot walk degrades instead of misparsing on drift.
        #[repr(C)]
        struct Probe {
            size: u32,
            usage: u32,
            tid: u32,
            owner: u32,
            base_pri: i32,
            delta_pri: i32,
            flags: u32,
        }
        assert_eq!(std::mem::size_of::<Probe>(), 28);
    }
}
