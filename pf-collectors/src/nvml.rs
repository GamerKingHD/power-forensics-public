//! Phase E vendor GPU seam: dynamic-load NVML without linking.
//!
//! Windows-only: `nvml.dll` is probed at runtime via raw
//! `LoadLibraryW`/`GetProcAddress` (no new dependencies). Only
//! `nvmlInit`/`nvmlShutdown` are resolved — NO power reads yet.
//! Missing DLL is the honest path on machines without NVIDIA GPUs
//! (e.g. ASUS iGPU box): `load()` returns `Err("nvml not present")`.
//! Non-Windows builds always return `Err("unsupported")`.

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(lp_lib_file_name: *const u16) -> usize;
    fn GetProcAddress(h_module: usize, lp_proc_name: *const u8) -> usize;
    fn FreeLibrary(h_lib_module: usize) -> i32;
}

/// Dynamically-loaded NVML library handle (init/shutdown symbols only).
pub struct Nvml {
    #[cfg(windows)]
    handle: usize,
    #[cfg(windows)]
    #[allow(dead_code)]
    init_fn: usize,
    #[cfg(windows)]
    shutdown_fn: usize,
}

#[cfg(windows)]
impl Nvml {
    /// Load `nvml.dll` and resolve `nvmlInit`/`nvmlShutdown` only.
    /// Never reads watts. `Err("nvml not present")` when the DLL or
    /// either symbol is missing.
    pub fn load() -> Result<Self, String> {
        let wide: Vec<u16> = "nvml.dll\0".encode_utf16().collect();
        // SAFETY: raw Win32 probe; null handle checked, freed on Drop.
        let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
        if handle == 0 {
            return Err("nvml not present".to_string());
        }
        let init_fn = unsafe { GetProcAddress(handle, c"nvmlInit".as_ptr() as *const u8) };
        let shutdown_fn = unsafe { GetProcAddress(handle, c"nvmlShutdown".as_ptr() as *const u8) };
        if init_fn == 0 || shutdown_fn == 0 {
            unsafe {
                FreeLibrary(handle);
            }
            return Err("nvml not present".to_string());
        }
        Ok(Nvml {
            handle,
            init_fn,
            shutdown_fn,
        })
    }

    fn shutdown_code(&self) -> i32 {
        // SAFETY: function pointer was validated non-null at load.
        unsafe {
            let f: unsafe extern "system" fn() -> i32 = std::mem::transmute(self.shutdown_fn);
            f()
        }
    }

    /// Probe: load + shutdown, never panics. True only when the DLL is
    /// present with both symbols; no watts are read.
    pub fn is_present() -> bool {
        match Self::load() {
            Ok(nvml) => {
                let _ = nvml.shutdown_code();
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(windows)]
impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: handle came from a successful LoadLibraryW.
        unsafe {
            FreeLibrary(self.handle);
        }
    }
}

#[cfg(not(windows))]
impl Nvml {
    pub fn load() -> Result<Self, String> {
        Err("unsupported".to_string())
    }

    pub fn is_present() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_dll_honesty_no_panic_no_watts() {
        match Nvml::load() {
            Ok(nvml) => {
                // NVML happens to be present: probe must agree, drop frees.
                assert!(Nvml::is_present());
                drop(nvml);
            }
            Err(e) => {
                assert!(!e.is_empty());
                #[cfg(windows)]
                assert!(e.contains("nvml not present"), "unexpected: {e}");
                #[cfg(not(windows))]
                assert!(e.contains("unsupported"), "unexpected: {e}");
                assert!(!Nvml::is_present());
            }
        }
    }

    #[test]
    fn is_present_never_panics() {
        let _ = Nvml::is_present();
    }
}
