//! Elevation probe (Windows). Mirrors the CLI's check so the GUI can explain
//! that elevation would unlock additional collectors without forcing it.

#[cfg(windows)]
#[link(name = "shell32")]
unsafe extern "system" {
    fn IsUserAnAdmin() -> i32;
}

pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        // SAFETY: argument-free getter.
        unsafe { IsUserAnAdmin() != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}
