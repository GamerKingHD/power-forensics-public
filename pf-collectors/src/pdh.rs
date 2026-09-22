//! Shared PDH (Performance Data Helper) access.
//!
//! One query object per collector. Counters are added by full path; an
//! add failure yields handle 0 (field degrades to Unavailable) instead of
//! failing the whole collector. Rate/percentage counters need two
//! collections before valid data — callers own their warmup.

#[cfg(windows)]
const PDH_FMT_DOUBLE: u32 = 0x200;
#[cfg(windows)]
const PDH_MORE_DATA: i32 = 0x800007D2u32 as i32;

#[cfg(windows)]
#[repr(C)]
struct FormattedValue {
    status: u32,
    _pad: u32,
    value: f64,
}

/// `PDH_FMT_COUNTERVALUE_ITEM_W`: one wildcard-expanded instance value.
#[cfg(windows)]
#[repr(C)]
struct FormattedItemW {
    name: *mut u16,
    value: FormattedValue,
}

#[cfg(windows)]
#[link(name = "pdh")]
unsafe extern "system" {
    fn PdhOpenQueryW(data_source: *const u16, user_data: usize, query: *mut usize) -> i32;
    /// Language-neutral counter lookup: the path is always English, so a
    /// localized Windows install cannot make CPU/GPU/storage/net counters
    /// silently disappear the way `PdhAddCounterW` would.
    fn PdhAddEnglishCounterW(
        query: usize,
        path: *const u16,
        user_data: usize,
        counter: *mut usize,
    ) -> i32;
    fn PdhCollectQueryData(query: usize) -> i32;
    fn PdhGetFormattedCounterValue(
        counter: usize,
        format: u32,
        ctype: *mut u32,
        value: *mut FormattedValue,
    ) -> i32;
    fn PdhGetFormattedCounterArrayW(
        counter: usize,
        format: u32,
        buffer_size: *mut u32,
        item_count: *mut u32,
        item_buffer: *mut FormattedItemW,
    ) -> i32;
    fn PdhCloseQuery(query: usize) -> i32;
}

/// The English counter used to expand `Object(*)` into instance names. Its
/// value is never consumed; only the returned instance names are.
#[cfg(windows)]
fn representative_counter(object: &str) -> Option<&'static str> {
    Some(match object {
        "Processor Information" => "% Processor Time",
        "Energy Meter" => "Power",
        "GPU Engine" => "Utilization Percentage",
        "GPU Process Memory" => "Dedicated Usage",
        "PhysicalDisk" => "% Idle Time",
        "Network Interface" => "Bytes Received/sec",
        _ => return None,
    })
}

/// Read a NUL-terminated UTF-16 string owned by PDH.
#[cfg(windows)]
unsafe fn wide_to_string(p: *const u16) -> String {
    let mut len = 0usize;
    // SAFETY: caller guarantees a NUL-terminated buffer.
    while unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Debug)]
pub struct PdhQuery {
    #[cfg(windows)]
    query: usize,
}

impl PdhQuery {
    #[cfg(windows)]
    pub fn open() -> Result<Self, String> {
        let mut query: usize = 0;
        // SAFETY: out-param handle, checked return.
        let rc = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) };
        if rc != 0 || query == 0 {
            return Err(format!("PdhOpenQueryW failed: {rc:#X}"));
        }
        Ok(PdhQuery { query })
    }

    #[cfg(not(windows))]
    pub fn open() -> Result<Self, String> {
        Err("Windows required".to_string())
    }

    /// Add a counter by its English full path (wildcards allowed). Returns 0
    /// when the path does not exist on this machine.
    #[cfg(windows)]
    pub fn add(&self, path: &str) -> usize {
        let w = wide(path);
        let mut handle: usize = 0;
        // SAFETY: query from open(), path is nul-terminated UTF-16.
        let rc = unsafe { PdhAddEnglishCounterW(self.query, w.as_ptr(), 0, &mut handle) };
        if rc == 0 && handle != 0 { handle } else { 0 }
    }

    #[cfg(not(windows))]
    pub fn add(&self, _path: &str) -> usize {
        0
    }

    #[cfg(windows)]
    pub fn collect(&self) -> Result<(), String> {
        // SAFETY: valid open query.
        let rc = unsafe { PdhCollectQueryData(self.query) };
        if rc != 0 {
            return Err(format!("PdhCollectQueryData failed: {rc:#X}"));
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn collect(&self) -> Result<(), String> {
        Err("Windows required".to_string())
    }

    /// Formatted double value. None for missing handles and not-ready
    /// rate counters (caller treats as Unavailable, never as 0).
    #[cfg(windows)]
    pub fn read_double(&self, handle: usize) -> Option<f64> {
        if handle == 0 {
            return None;
        }
        let mut ctype: u32 = 0;
        let mut val = FormattedValue {
            status: 0,
            _pad: 0,
            value: 0.0,
        };
        // SAFETY: handle from add(), stack out-params.
        let rc =
            unsafe { PdhGetFormattedCounterValue(handle, PDH_FMT_DOUBLE, &mut ctype, &mut val) };
        if rc == 0 && val.status == 0 {
            Some(val.value)
        } else {
            None
        }
    }

    #[cfg(not(windows))]
    pub fn read_double(&self, _handle: usize) -> Option<f64> {
        None
    }

    /// Wildcard-expanded instance values for a counter added with `(*)`.
    /// Names are returned even when a value is not ready (None), so instance
    /// enumeration never depends on a valid first rate sample.
    #[cfg(windows)]
    pub fn read_array(&self, handle: usize) -> Vec<(String, Option<f64>)> {
        if handle == 0 {
            return Vec::new();
        }
        let mut size: u32 = 0;
        let mut count: u32 = 0;
        // SAFETY: size query with null buffer; PDH_MORE_DATA is expected.
        let rc = unsafe {
            PdhGetFormattedCounterArrayW(
                handle,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        if rc != PDH_MORE_DATA || size == 0 || count == 0 {
            return Vec::new();
        }
        // Vec<u64> guarantees the 8-byte alignment the item struct needs.
        let words = (size as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buf: Vec<u64> = vec![0; words];
        // SAFETY: buffer sized by the previous call.
        let rc = unsafe {
            PdhGetFormattedCounterArrayW(
                handle,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                buf.as_mut_ptr() as *mut FormattedItemW,
            )
        };
        if rc != 0 {
            return Vec::new();
        }
        let items = buf.as_ptr() as *const FormattedItemW;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            // SAFETY: PDH reported `count` items in the sized buffer.
            let item = unsafe { &*items.add(i) };
            let name = if item.name.is_null() {
                String::new()
            } else {
                // SAFETY: PDH-owned nul-terminated instance name.
                unsafe { wide_to_string(item.name) }
            };
            let value = if item.value.status == 0 {
                Some(item.value.value)
            } else {
                None
            };
            out.push((name, value));
        }
        out
    }

    #[cfg(not(windows))]
    pub fn read_array(&self, _handle: usize) -> Vec<(String, Option<f64>)> {
        Vec::new()
    }

    /// Instance names of an English PDH object (e.g. "GPU Engine",
    /// "Energy Meter"). Uses the wildcard + array path so localized object
    /// names on non-English Windows cannot break enumeration.
    #[cfg(windows)]
    pub fn enum_instances(object_name: &str) -> Result<Vec<String>, String> {
        let counter = representative_counter(object_name)
            .ok_or_else(|| format!("no representative English counter for object {object_name}"))?;
        let query = PdhQuery::open()?;
        let path = format!("\\{object_name}(*)\\{counter}");
        let handle = query.add(&path);
        if handle == 0 {
            return Err(format!("PdhAddEnglishCounterW failed: {path}"));
        }
        // Two collects: rate counters need a prior sample; instance names are
        // present regardless of value readiness.
        query.collect()?;
        query.collect()?;
        let names: Vec<String> = query
            .read_array(handle)
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| !n.is_empty())
            .collect();
        if names.is_empty() {
            return Err(format!("no instances enumerated for {path}"));
        }
        Ok(names)
    }

    #[cfg(not(windows))]
    pub fn enum_instances(_object_name: &str) -> Result<Vec<String>, String> {
        Err("Windows required".to_string())
    }
}

#[cfg(windows)]
impl Drop for PdhQuery {
    fn drop(&mut self) {
        if self.query != 0 {
            // SAFETY: valid open query, teardown path.
            unsafe {
                PdhCloseQuery(self.query);
            }
            self.query = 0;
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Integration test against the live PDH subsystem: the English wildcard
    /// path must enumerate real instances on this machine.
    #[test]
    fn english_enumeration_finds_processor_instances() {
        let insts = PdhQuery::enum_instances("Processor Information").expect("enum");
        assert!(insts.iter().any(|i| i.eq_ignore_ascii_case("_total")));
    }

    #[test]
    fn english_counter_add_works() {
        let q = PdhQuery::open().expect("open");
        assert_ne!(q.add("\\System\\Context Switches/sec"), 0);
    }

    /// An object with no representative mapping is reported, not guessed.
    #[test]
    fn unknown_object_is_reported() {
        assert!(PdhQuery::enum_instances("No Such Object").is_err());
    }
}
