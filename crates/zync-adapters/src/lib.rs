//! Everything outside the process: the light network, the screen, and the files
//! on disk. Depends on `zync-core`; nothing in `zync-core` depends on this.

use anyhow::Result;
use zync_core::ports::FrameSource;

pub mod config;
pub mod lights;
pub mod mqtt;

mod capture;

/// Opens the frame source for this platform. Which backend that is gets decided
/// inside [`capture`]; nothing above this line changes per platform.
pub fn open_frame_source() -> Result<Box<dyn FrameSource>> {
    capture::open()
}
