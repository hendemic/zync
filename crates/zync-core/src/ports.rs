//! The boundaries of the application. Read this file to understand the
//! architecture: everything outside the process reaches the sync loop through
//! one of these two traits.
//!
//! Both are deliberately synchronous and *coarse* — one call per frame, one per
//! light command. At that granularity a blocking call costs nothing, and an
//! adapter that needs async internally can own its own runtime behind the trait
//! without pushing `Send + Sync + 'static` through every layer above it.

use anyhow::Result;

use crate::domain::{Frame, LightCommand, LightId};

/// A source of screen frames. Implemented once per platform.
///
/// Teardown is `Drop`, not a method: a session that ends must release its
/// compositor resources even when it ends by error or panic.
pub trait FrameSource: Send {
    /// The most recent frame. May be scaled down relative to [`Self::source_size`].
    fn next_frame(&self) -> Result<Frame>;

    /// Native resolution of the capture source. Zones are configured in this
    /// coordinate space no matter what resolution frames actually arrive at.
    fn source_size(&self) -> (u32, u32);

    /// Total frames the source has delivered. Lets the sync loop tell a stalled
    /// stream apart from a screen that simply is not changing — from the outside
    /// those two look identical, and only one of them is a bug.
    fn frames_captured(&self) -> u64;

    /// Human-readable summary of how frames are being obtained, for startup output.
    fn describe(&self) -> String;
}

/// A light network. Owns every configured light rather than one apiece, so the
/// sync loop can hold a single mutable handle and address lights by id.
pub trait LightSink: Send {
    /// Whether [`Self::send`] would actually transmit. Checked before spending
    /// pacing budget, since a change visible in a sample can still round to the
    /// command the light already holds.
    fn would_send(&self, light: &LightId, command: LightCommand) -> bool;

    /// Sends a command. Returns whether anything went out; a call that dedupes
    /// away is `Ok(false)` and must not be charged against the budget.
    fn send(&mut self, light: &LightId, command: LightCommand) -> Result<bool>;

    /// Delivery failures the network has reported for this light. Monotonic since
    /// the sink was created; the sync loop diffs against what it has absorbed.
    ///
    /// Publishes are fire-and-forget, so this is the only signal that the mesh is
    /// refusing work, and the only thing the pacing can honestly react to.
    fn failures(&self, light: &LightId) -> u64;

    /// Records what the lights are currently showing, so [`Self::restore`] can
    /// put them back. Best effort: lights that do not answer are reported and
    /// fall back to their configured state.
    fn snapshot(&mut self) -> Result<()>;

    /// Applies the configured stop policy. Called once as a session ends.
    fn restore(&mut self) -> Result<()>;
}
