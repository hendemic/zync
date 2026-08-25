//! Screen capture, one backend per platform, all behind [`FrameSource`].
//!
//! This is the only platform-specific part of the app. Each backend owns its
//! own native dependencies and threading; nothing above this module knows which
//! one is running.

use anyhow::Result;
use zync_core::ports::FrameSource;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

/// Opens the frame source for this platform.
pub fn open() -> Result<Box<dyn FrameSource>> {
    #[cfg(target_os = "linux")]
    {
        linux::open()
    }
    #[cfg(target_os = "macos")]
    {
        macos::open()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!(
            "Screen capture is not implemented for this platform yet. Linux (Wayland and X11) and macOS are supported."
        )
    }
}
