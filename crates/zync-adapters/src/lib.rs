//! Everything outside the process: the light network, the screen, and the files
//! on disk. Depends on `zync-core`; nothing in `zync-core` depends on this.

use anyhow::Result;
use zync_core::ports::FrameSource;

pub mod config;
pub mod lights;
pub mod mqtt;

#[cfg(target_os = "linux")]
mod capture;

/// Opens the frame source for this platform.
///
/// The single place platform support is decided. macOS and Windows backends slot
/// in here behind the same port, with no change above this line.
pub fn open_frame_source() -> Result<Box<dyn FrameSource>> {
    #[cfg(target_os = "linux")]
    {
        capture::open()
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!(
            "Screen capture is not implemented for this platform yet. Linux (Wayland and X11) is supported."
        )
    }
}
