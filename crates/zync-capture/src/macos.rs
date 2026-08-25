//! macOS screen capture through ScreenCaptureKit, exposed as a [`FrameSource`].
//!
//! This is built on the `objc2-*` bindings rather than the `screencapturekit`
//! crate. That crate bridges through Swift, which drags in a pinned SDK, a
//! `swift build` step and a `build.rs` in every consumer binary; the capture
//! path here is about eight API calls, so plain FFI declarations are cheaper to
//! own than that toolchain. See `dev-notes/macos-sck-spike/FINDINGS.md`.
//!
//! ScreenCaptureKit pushes frames at us on its own dispatch queue rather than
//! answering a pull, so the shape mirrors the Wayland backend: the callback
//! publishes into shared state and the sync loop reads the newest frame.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send, sel};
use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID, CGPreflightScreenCaptureAccess,
    CGRequestScreenCaptureAccess,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCCaptureDynamicRange, SCContentFilter, SCDisplay, SCShareableContent, SCStream,
    SCStreamConfiguration, SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};
use tracing::{debug, warn};
use zync_core::domain::Frame;
use zync_core::ports::FrameSource;

/// Averaging a zone's colour needs almost no spatial detail, so ScreenCaptureKit
/// is asked to scale frames down before we ever see them rather than handing
/// over native-resolution buffers. A 4K frame costs 31.6 MiB; this costs
/// 0.9 MiB. Zone configuration is unaffected because zones are declared in
/// native display coordinates and translated by the domain's zone sampler.
const CAPTURE_WIDTH: usize = 640;
const CAPTURE_HEIGHT: usize = 360;

/// Ceiling on frames per second, expressed as the stream's minimum frame
/// interval so ScreenCaptureKit throttles *before* doing any work on our
/// behalf. Left unset it paces to the display's refresh rate, and every one of
/// those frames costs a scale and a surface round-trip inside the window
/// server. Thirty is well past what the lights can act on.
const CAPTURE_MAX_FPS: i32 = 30;

/// RGBA and BGRA are both four bytes; the domain's `Frame` agrees but keeps the
/// constant private.
const BYTES_PER_PIXEL: usize = 4;

/// Listing shareable content talks to the window server, which is also the
/// process that arbitrates the screen-recording grant. A denied or still-being
/// decided grant is the slow case, so this is generous rather than snappy.
const SHAREABLE_CONTENT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long ScreenCaptureKit gets to acknowledge the start before we give up.
/// The stream allocates its surface pool and attaches to the display here.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// Upper bound on waiting for the stream's first frame once it has started.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// How long teardown waits for the stop to complete. Short on purpose: this
/// runs on the shutdown path, and the only cost of giving up early is that the
/// last few callbacks are dropped.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Opens the main display's frame source.
pub fn open() -> Result<Box<dyn FrameSource>> {
    MacosSource::open().map(|source| Box::new(source) as Box<dyn FrameSource>)
}

/// Shared between ScreenCaptureKit's callback threads and the sync loop.
#[derive(Default)]
struct StreamState {
    /// Frames are cheap to clone, so the sync loop takes one without copying
    /// pixels or holding the lock while it samples.
    frame: Option<Frame>,
    /// Set when the stream stops or delivers something unreadable, so startup
    /// fails instead of hanging and a mid-run failure is reported rather than
    /// looking like a still screen.
    error: Option<String>,
}

/// A running ScreenCaptureKit stream.
struct MacosSource {
    state: Arc<Mutex<StreamState>>,
    /// Counted outside the mutex so the callback threads never block to report.
    frames: Arc<AtomicU64>,
    source_size: (u32, u32),
    stream: SendStream,
    /// `SCStream` holds its delegate weakly, so it has to be kept alive here or
    /// the stop-with-error callback would never arrive. The output handler and
    /// its queue are retained for the same reason: neither should be able to
    /// die while ScreenCaptureKit is still dispatching to them.
    _delegate: Retained<StreamDelegate>,
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

/// The `SCStream` handle, made movable between threads.
///
/// objc2 leaves every generated framework class `!Send`, because thread safety
/// is a per-class property it cannot infer from a header. That is a problem
/// here only because the sync loop owns the [`FrameSource`] on whichever thread
/// it runs on and drops it there, so the handle has to travel.
///
/// Three things make that sound. The stream is only ever touched twice —
/// started at the end of [`MacosSource::open`] and stopped in `Drop` — and
/// ownership orders those, so there is no concurrent use to race. Its whole API
/// is asynchronous with completion handlers and none of it is main-thread-only,
/// which is why objc2 does not mark the class `MainThreadOnly` either. And the
/// spike in `dev-notes/macos-sck-spike` did exactly this deliberately: stopped
/// and released a stream from a thread other than the one that created it.
///
/// Nothing else in this module needs the same treatment: the delegate and
/// output handler are classes defined here, whose ivars are `Arc`s of locks and
/// atomics, and dispatch queues are thread-safe by construction.
struct SendStream(Retained<SCStream>);

// SAFETY: see the type's documentation.
unsafe impl Send for SendStream {}

impl MacosSource {
    fn open() -> Result<Self> {
        ensure_permission()?;

        let content = shareable_content()?;
        let display = main_display(&content)?;
        let source_size = native_pixel_size(&display)?;

        let state = Arc::new(Mutex::new(StreamState::default()));
        let frames = Arc::new(AtomicU64::new(0));

        let filter = content_filter(&display);
        let config = stream_configuration();
        let delegate = StreamDelegate::new(Arc::clone(&state));
        let output = StreamOutput::new(Arc::clone(&state), Arc::clone(&frames));

        // Serial: frames are handled in order, and one copy cannot overlap the
        // next. ScreenCaptureKit otherwise spreads callbacks across several
        // threads of its own, which buys nothing when the work is a memcpy.
        let queue = DispatchQueue::new("com.zync.capture", DispatchQueueAttr::SERIAL);

        // SAFETY: the filter, configuration and delegate are all live for the
        // duration of the call, and this is an `init` on a fresh allocation.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*delegate)),
            )
        };

        // SAFETY: `output` implements SCStreamOutput and is kept alive for as
        // long as the stream is, and `queue` is an ordinary serial dispatch
        // queue, which is what the sample handler expects.
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| {
            anyhow!(
                "ScreenCaptureKit refused the frame handler: {}",
                e.localizedDescription()
            )
        })?;

        start_capture(&stream)?;

        let source = MacosSource {
            state,
            frames,
            source_size,
            stream: SendStream(stream),
            _delegate: delegate,
            _output: output,
            _queue: queue,
        };

        // On failure this returns through `Drop`, which stops the stream, so a
        // stream that starts but never delivers does not leak into the session.
        source.await_first_frame()?;

        Ok(source)
    }

    /// Blocks until the stream publishes a frame, failing fast if the delegate
    /// reported a stop rather than spinning out the whole timeout.
    fn await_first_frame(&self) -> Result<()> {
        let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;

        loop {
            {
                let guard = self
                    .state
                    .lock()
                    .map_err(|_| anyhow!("Capture state lock poisoned"))?;

                if let Some(error) = &guard.error {
                    bail!("Screen capture stream failed: {error}");
                }

                if guard.frame.is_some() {
                    return Ok(());
                }
            }

            if Instant::now() >= deadline {
                bail!("Timed out waiting for the first frame from ScreenCaptureKit");
            }

            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl FrameSource for MacosSource {
    fn next_frame(&self) -> Result<Frame> {
        let guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("Capture state lock poisoned"))?;

        if let Some(error) = &guard.error {
            bail!("Screen capture stream failed: {error}");
        }

        guard
            .frame
            .clone()
            .ok_or_else(|| anyhow!("No frame available"))
    }

    fn source_size(&self) -> (u32, u32) {
        self.source_size
    }

    fn frames_captured(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn describe(&self) -> String {
        "ScreenCaptureKit, scaled by the compositor".to_string()
    }
}

impl Drop for MacosSource {
    fn drop(&mut self) {
        let (tx, rx) = mpsc::channel();
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let _ = tx.send(());
        });

        // SAFETY: the block's argument type matches the completion handler
        // ScreenCaptureKit declares, and `RcBlock` copies it to the heap so it
        // outlives this call. The handler may run on another thread; all it
        // captures is a channel sender used exactly once.
        unsafe { self.stream.0.stopCaptureWithCompletionHandler(Some(&handler)) };

        // Waited on rather than fired and forgotten: the output handler and its
        // queue are released the moment this value finishes dropping, and
        // ScreenCaptureKit is still dispatching to both until the stop lands.
        if rx.recv_timeout(STOP_TIMEOUT).is_err() {
            debug!("ScreenCaptureKit did not confirm the capture stop");
        }
    }
}

/// Fails unless this process may record the screen, raising the system prompt
/// on the way if macOS has not asked the user yet.
fn ensure_permission() -> Result<()> {
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }

    // Prompts only while the decision is still undecided; afterwards it just
    // reports the stored answer, so this is safe to call on every start.
    if CGRequestScreenCaptureAccess() {
        return Ok(());
    }

    bail!(
        "zync is not permitted to record the screen. Enable it under System Settings -> \
         Privacy & Security -> Screen & System Audio Recording, then run zync again. \
         macOS files the grant against whatever launched zync, so the entry to enable is \
         the terminal you started it from - or zync itself once launchd is starting it."
    )
}

/// The set of displays ScreenCaptureKit is willing to capture.
///
/// The API is completion-handler only and answers on a queue of its own
/// choosing, so the result is bridged back over a channel and waited on here.
fn shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        // SAFETY: ScreenCaptureKit passes either a live SCShareableContent or a
        // live NSError, each valid only for this call. The content is retained
        // so it outlives the handler; the error is copied into an owned String.
        let result = unsafe {
            match (Retained::retain(content), error.as_ref()) {
                (Some(content), _) => Ok(SendContent(content)),
                (None, Some(error)) => Err(error.localizedDescription().to_string()),
                (None, None) => Err("neither content nor an error was returned".to_string()),
            }
        };
        let _ = tx.send(result);
    });

    // SAFETY: the block's argument types match the completion handler
    // ScreenCaptureKit declares, and `RcBlock` copies it to the heap so it
    // outlives this call. ScreenCaptureKit answers on a queue of its own, so
    // the handler may run on another thread; everything it captures is a
    // channel sender used exactly once.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };

    rx.recv_timeout(SHAREABLE_CONTENT_TIMEOUT)
        .context("Timed out asking ScreenCaptureKit which displays are shareable")?
        .map(|content| content.0)
        .map_err(|e| anyhow!("ScreenCaptureKit would not list the displays: {e}"))
}

/// Carries the shareable-content snapshot from the thread ScreenCaptureKit ran
/// the completion handler on back to the thread that asked for it.
///
/// The snapshot is immutable — it exposes only getters over lists captured when
/// the query ran — and Objective-C reference counting is atomic, so reading it
/// and releasing it on another thread are both fine. objc2 cannot know that, so
/// it says so here rather than making the type `Send` everywhere.
struct SendContent(Retained<SCShareableContent>);

// SAFETY: see the type's documentation.
unsafe impl Send for SendContent {}

/// Picks the display macOS considers the main one, logging the rest.
fn main_display(content: &SCShareableContent) -> Result<Retained<SCDisplay>> {
    let main = CGMainDisplayID();
    // SAFETY: a property read on the immutable snapshot.
    let displays = unsafe { content.displays() };

    // Named `screen` rather than `display` because `tracing`'s field syntax
    // reserves that word for its own value formatter.
    displays.iter().for_each(|screen| {
        // SAFETY: property reads on the immutable snapshot. Width and height
        // are points here, which is why they are only logged and never used as
        // the source size.
        unsafe {
            debug!(
                id = screen.displayID(),
                points_wide = screen.width(),
                points_high = screen.height(),
                is_main = screen.displayID() == main,
                "shareable display"
            );
        }
    });

    displays
        // SAFETY: a property read on the immutable snapshot.
        .iter()
        .find(|display| unsafe { display.displayID() } == main)
        .ok_or_else(|| {
            anyhow!("ScreenCaptureKit did not offer the main display ({main}) for capture")
        })
}

/// The display's backing-store size in real pixels.
///
/// `SCDisplay`'s own width and height are *points*, which on a Retina panel are
/// half the pixels. Zones are configured against the pixel grid the user sees
/// in a screenshot, so the display mode is the authority.
fn native_pixel_size(display: &SCDisplay) -> Result<(u32, u32)> {
    // SAFETY: a property read on the immutable snapshot.
    let id = unsafe { display.displayID() };
    let mode = CGDisplayCopyDisplayMode(id)
        .with_context(|| format!("Display {id} reported no current display mode"))?;

    let pixels = (
        CGDisplayMode::pixel_width(Some(&mode)),
        CGDisplayMode::pixel_height(Some(&mode)),
    );

    let (width, height) = match pixels {
        (0, _) | (_, 0) => {
            warn!(id, "display mode gave no pixel size; falling back to points");
            // SAFETY: property reads on the immutable snapshot.
            unsafe { (display.width().max(0) as usize, display.height().max(0) as usize) }
        }
        dimensions => dimensions,
    };

    if width == 0 || height == 0 {
        bail!("Display {id} reported a zero size");
    }

    Ok((width as u32, height as u32))
}

/// Captures the whole display, excluding no windows.
fn content_filter(display: &SCDisplay) -> Retained<SCContentFilter> {
    // SAFETY: an `init` on a fresh allocation; the empty array names no windows
    // to exclude, so the filter is the display in full.
    unsafe {
        SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            display,
            &NSArray::new(),
        )
    }
}

fn stream_configuration() -> Retained<SCStreamConfiguration> {
    // SAFETY: `new` is NSObject's, and each call below is a plain property
    // write on the configuration object just created.
    unsafe {
        let config = SCStreamConfiguration::new();

        config.setWidth(CAPTURE_WIDTH);
        config.setHeight(CAPTURE_HEIGHT);
        // Set explicitly because the runtime default is not BGRA: macOS 26
        // hands out biplanar YUV ('420v') unless told otherwise, which the
        // sampler cannot read.
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        config.setMinimumFrameInterval(CMTime::new(1, CAPTURE_MAX_FPS));
        // The pointer is not part of the picture being sampled, and moving it
        // would otherwise mark the screen changed on every twitch.
        config.setShowsCursor(false);

        // Off so a display whose shape is not 16:9 is stretched into the frame
        // rather than letterboxed: bars would be averaged into the edge zones as
        // black. Stretching is harmless because zones are scaled per axis, so a
        // zone still covers the same fraction of the screen either way. The
        // property arrived in macOS 14, defaulting to on, hence the guard.
        if config.respondsToSelector(sel!(setPreservesAspectRatio:)) {
            config.setPreservesAspectRatio(false);
        }

        // `captureDynamicRange` arrived in macOS 15. SDR is the documented
        // default, so on anything older — where sending this selector would
        // raise — leaving it alone is already correct.
        if config.respondsToSelector(sel!(setCaptureDynamicRange:)) {
            config.setCaptureDynamicRange(SCCaptureDynamicRange::SDR);
        }

        config
    }
}

/// Starts the stream and waits for its completion handler, so a refused start
/// surfaces as an error here rather than as "no frames" thirty seconds later.
fn start_capture(stream: &SCStream) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: the pointer is either null or a live NSError for the duration
        // of the call; its message is copied out before returning.
        let message = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send(message);
    });

    // SAFETY: the block's argument type matches the completion handler
    // ScreenCaptureKit declares, and `RcBlock` copies it to the heap so it
    // outlives this call. The handler may run on another thread; all it
    // captures is a channel sender used exactly once.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };

    match rx.recv_timeout(START_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(message)) => bail!("ScreenCaptureKit could not start the capture: {message}"),
        Err(_) => bail!("ScreenCaptureKit did not answer the capture start in {START_TIMEOUT:?}"),
    }
}

/// What the sample-buffer callback needs in order to publish a frame.
struct OutputIvars {
    state: Arc<Mutex<StreamState>>,
    frames: Arc<AtomicU64>,
}

define_class!(
    // SAFETY:
    // - NSObject imposes no subclassing requirements.
    // - StreamOutput does not implement Drop.
    #[unsafe(super(NSObject))]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    // SAFETY: the selector and its argument types are the ones SCStreamOutput
    // declares, and the method body is safe to run on any thread.
    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output_sample_buffer(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }

            // SAFETY: `sample_buffer` is live for this call, and the image
            // buffer it returns is retained for as long as `pixels` lives.
            let Some(pixels) = (unsafe { sample_buffer.image_buffer() }) else {
                // A sample with no image buffer is ScreenCaptureKit saying the
                // screen has not changed. Deliberately not counted: the sync
                // loop reads the counter to tell a stalled stream apart from a
                // screen that is simply still.
                return;
            };

            let format = CVPixelBufferGetPixelFormatType(&pixels);
            if format != kCVPixelFormatType_32BGRA {
                // Recorded once rather than warned about per frame: the format
                // was requested explicitly, so a mismatch means the OS
                // overrode it and every frame from here is unreadable.
                self.record_error(format!(
                    "stream delivered pixel format {format:#010x}, not BGRA"
                ));
                return;
            }

            match read_frame(&pixels) {
                Ok(frame) => {
                    if let Ok(mut guard) = self.ivars().state.lock() {
                        guard.frame = Some(frame);
                        self.ivars().frames.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => warn!(error = %format!("{e:#}"), "discarding an unusable frame"),
            }
        }
    }
);

impl StreamOutput {
    fn new(state: Arc<Mutex<StreamState>>, frames: Arc<AtomicU64>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { state, frames });
        // SAFETY: NSObject's designated initialiser, on a fresh allocation
        // whose ivars have just been set.
        unsafe { msg_send![super(this), init] }
    }

    /// Records the first fatal problem seen by the callback. Later ones are
    /// dropped: the first is the one that explains the rest.
    fn record_error(&self, message: String) {
        if let Ok(mut guard) = self.ivars().state.lock() {
            guard.error.get_or_insert(message);
        }
    }
}

/// What the delegate needs in order to report a stopped stream.
struct DelegateIvars {
    state: Arc<Mutex<StreamState>>,
}

define_class!(
    // SAFETY:
    // - NSObject imposes no subclassing requirements.
    // - StreamDelegate does not implement Drop.
    #[unsafe(super(NSObject))]
    #[ivars = DelegateIvars]
    struct StreamDelegate;

    unsafe impl NSObjectProtocol for StreamDelegate {}

    // SAFETY: the selector and its argument types are the ones SCStreamDelegate
    // declares, and the method body is safe to run on any thread.
    unsafe impl SCStreamDelegate for StreamDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            let message = error.localizedDescription().to_string();
            warn!(error = %message, "ScreenCaptureKit stopped the stream");

            if let Ok(mut guard) = self.ivars().state.lock() {
                guard.error = Some(message);
            }
        }
    }
);

impl StreamDelegate {
    fn new(state: Arc<Mutex<StreamState>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { state });
        // SAFETY: NSObject's designated initialiser, on a fresh allocation
        // whose ivars have just been set.
        unsafe { msg_send![super(this), init] }
    }
}

/// Copies one BGRA pixel buffer into a tightly packed RGBA [`Frame`].
///
/// The buffer belongs to ScreenCaptureKit's surface pool and is handed back the
/// moment the callback returns, so the pixels are copied out rather than
/// borrowed.
fn read_frame(pixels: &CVPixelBuffer) -> Result<Frame> {
    let guard = LockedPixels::acquire(pixels)?;

    let width = CVPixelBufferGetWidth(pixels);
    let height = CVPixelBufferGetHeight(pixels);
    let stride = CVPixelBufferGetBytesPerRow(pixels);
    let base = CVPixelBufferGetBaseAddress(pixels);

    if width == 0 || height == 0 {
        bail!("Frame was {width}x{height}");
    }
    if base.is_null() {
        bail!("Locked frame has no base address");
    }

    // The stride is read rather than assumed: IOSurface pads rows to its own
    // alignment, and the spike's exact `width * 4` was a coincidence of this
    // machine's geometry.
    let row_bytes = width
        .checked_mul(BYTES_PER_PIXEL)
        .context("Frame width overflows a row")?;
    if stride < row_bytes {
        bail!("Frame stride {stride} is narrower than its {row_bytes}-byte rows");
    }
    let mapped = stride
        .checked_mul(height - 1)
        .and_then(|full| full.checked_add(row_bytes))
        .context("Frame dimensions overflow")?;

    // SAFETY: `guard` holds the buffer locked for reading, and CoreVideo
    // guarantees `height` rows of `stride` bytes from the base address. Only
    // the first `row_bytes` of the last row are covered, which is all that is
    // read below, so no trailing padding is touched.
    let bytes = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), mapped) };

    // ScreenCaptureKit delivers BGRA and `Frame` expects RGBA, so the copy
    // swizzles as it goes. Row padding is dropped here instead of carried as a
    // stride: the frame is small enough that packing it is cheaper than making
    // every consumer honour a pitch.
    // Sized up front: nested flat_maps carry no size hint, and growing a 0.9 MiB
    // vector by doubling thirty times a second is avoidable work.
    let mut data = Vec::with_capacity(row_bytes * height);
    data.extend(
        bytes
            .chunks(stride)
            .take(height)
            .flat_map(|row| row.chunks_exact(BYTES_PER_PIXEL).take(width))
            .flat_map(|bgra| [bgra[2], bgra[1], bgra[0], bgra[3]]),
    );

    drop(guard);

    Frame::from_packed_rgba(data, width as u32, height as u32)
        .context("ScreenCaptureKit frame was not usable")
}

/// A `CVPixelBuffer` mapped for CPU reading.
///
/// The lock is reference-counted by CoreVideo, so an early return that skipped
/// the unlock would keep the surface out of the stream's pool permanently. A
/// guard makes that impossible to get wrong.
struct LockedPixels<'a> {
    buffer: &'a CVPixelBuffer,
}

impl<'a> LockedPixels<'a> {
    fn acquire(buffer: &'a CVPixelBuffer) -> Result<Self> {
        // SAFETY: `buffer` is a live CVPixelBuffer for all of `'a`, and the
        // matching unlock is guaranteed by this type's `Drop`.
        let status =
            unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) };

        if status != kCVReturnSuccess {
            bail!("Could not lock the frame for reading (CVReturn {status})");
        }

        Ok(LockedPixels { buffer })
    }
}

impl Drop for LockedPixels<'_> {
    fn drop(&mut self) {
        // SAFETY: pairs with the successful lock in `acquire`, with the same
        // flags, on the same still-live buffer.
        let status = unsafe {
            CVPixelBufferUnlockBaseAddress(self.buffer, CVPixelBufferLockFlags::ReadOnly)
        };

        if status != kCVReturnSuccess {
            warn!(status, "could not unlock a frame after reading it");
        }
    }
}
