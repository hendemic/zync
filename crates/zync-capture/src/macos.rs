//! macOS screen capture through ScreenCaptureKit, exposed as a [`FrameSource`].

use anyhow::{Result, bail};
use zync_core::ports::FrameSource;

pub fn open() -> Result<Box<dyn FrameSource>> {
    bail!("macOS capture backend is not wired up yet")
}
