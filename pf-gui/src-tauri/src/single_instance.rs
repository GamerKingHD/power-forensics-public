//! Per-user GUI singleton.
//!
//! Two GUI windows attached to the same agent could both issue Pause/Resume/
//! Stop, which is ambiguous control authority. For the initial release the GUI
//! is single-instance per login session: the first window owns the agent
//! controls; a second launch focuses it (or exits cleanly). The agent keeps its
//! own separate singleton lock; this module never touches it.

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(
            attrs: *mut core::ffi::c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut core::ffi::c_void;
        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
        fn GetLastError() -> u32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn FindWindowW(class: *const u16, title: *const u16) -> *mut core::ffi::c_void;
        fn SetForegroundWindow(hwnd: *mut core::ffi::c_void) -> i32;
        fn ShowWindow(hwnd: *mut core::ffi::c_void, cmd: i32) -> i32;
    }

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const SW_RESTORE: i32 = 9;

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Held for the process lifetime; releasing closes the mutex.
    pub struct Guard(*mut core::ffi::c_void);

    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// `Some(guard)` for the first instance, `None` when another owns the name.
    pub fn acquire(name: &str) -> Option<Guard> {
        let wide_name = wide(name);
        let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide_name.as_ptr()) };
        if handle.is_null() {
            // Could not create the primitive; fail open so the GUI still runs.
            return Some(Guard(std::ptr::null_mut()));
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            return None;
        }
        Some(Guard(handle))
    }

    /// Best-effort restore/foreground of the existing window.
    pub fn focus_existing(title: &str) {
        let wide_title = wide(title);
        let hwnd = unsafe { FindWindowW(std::ptr::null(), wide_title.as_ptr()) };
        if hwnd.is_null() {
            return;
        }
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
}

#[cfg(windows)]
pub use win::Guard;

/// Acquire the GUI singleton for this login session.
#[cfg(windows)]
pub fn acquire_gui() -> Option<Guard> {
    win::acquire("Local\\power-forensics-gui")
}

#[cfg(windows)]
pub fn focus_existing_gui() {
    win::focus_existing("power-forensics");
}

#[cfg(not(windows))]
pub struct Guard;

#[cfg(not(windows))]
pub fn acquire_gui() -> Option<Guard> {
    Some(Guard)
}

#[cfg(not(windows))]
pub fn focus_existing_gui() {}
