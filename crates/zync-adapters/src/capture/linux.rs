//! Linux screen capture: a PipeWire screencast on Wayland, plain screenshots on
//! X11. Both are exposed as a [`FrameSource`].

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use gst_video::prelude::*;
use gstreamer as gst;
use gstreamer_app::{AppSink, AppSinkCallbacks};
use gstreamer_video as gst_video;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tracing::{debug, info, warn};
use xcap::Monitor;
use zync_core::domain::Frame;
use zync_core::ports::FrameSource;

use crate::config::State;

/// Averaging a zone's colour needs almost no spatial detail, so frames are scaled
/// down inside the pipeline rather than hauling native-resolution buffers around.
/// A 4K frame costs 31.6 MiB; this costs 0.9 MiB. Zone configuration is unaffected
/// because zones are declared in native display coordinates and translated by the
/// domain's zone sampler.
const CAPTURE_WIDTH: i32 = 640;
const CAPTURE_HEIGHT: i32 = 360;

/// Ceiling on frames per second, negotiated with the compositor as the stream's
/// max-framerate so it throttles *before* doing any work on our behalf. Mutter
/// otherwise defaults this to the monitor refresh rate and performs a scanout
/// copy plus a PipeWire buffer round-trip for every one of them, inside
/// gnome-shell — at 240Hz that is most of the screencast's CPU cost. videorate
/// enforces the same ceiling locally for compositors that ignore the hint.
const CAPTURE_MAX_FPS: i32 = 30;

/// Portal negotiation blocks on the user picking a monitor, so this is generous.
const PORTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a capture mode gets to deliver its first frame before it is judged
/// unsupported and the next mode is tried. GL context creation and PipeWire
/// format renegotiation both happen inside this window.
const MODE_NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(8);

/// Upper bound on startup across every capture mode attempt.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// Which kind of buffer the compositor is asked to fill.
///
/// This is not a performance knob; it decides whether fullscreen windows are
/// captured at all on GNOME. Mutter only records a directly-scanned-out frame
/// (fullscreen games, fullscreen video) for streams that negotiated DMA-BUFs:
/// its `before_stage_painted` handler returns early for shared-memory streams,
/// and the stage is never painted under scanout, so nothing else fires either.
/// Shared memory also forces a full-resolution GPU→CPU readback inside
/// gnome-shell on every frame, which DMA-BUF avoids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureMode {
    /// DMA-BUF buffers imported straight into GL and scaled on the GPU.
    DmaBufGpu,
    /// Shared-memory buffers scaled on the CPU. Fallback for compositors or
    /// installs that cannot negotiate the above.
    SharedMemory,
}

impl CaptureMode {
    fn describe(self) -> &'static str {
        match self {
            CaptureMode::DmaBufGpu => "DMA-BUF, scaled on the GPU",
            CaptureMode::SharedMemory => {
                "shared memory, scaled on the CPU (fullscreen apps will not be captured on GNOME)"
            }
        }
    }
}

/// Opens the display's frame source, preferring the Wayland screencast portal.
pub fn open() -> Result<Box<dyn FrameSource>> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        WaylandSource::open().map(|source| Box::new(source) as Box<dyn FrameSource>)
    } else {
        X11Source::open().map(|source| Box::new(source) as Box<dyn FrameSource>)
    }
}

/// X11 screenshots of the primary monitor.
struct X11Source {
    monitor: Monitor,
    source_size: (u32, u32),
    frames: AtomicU64,
}

impl X11Source {
    fn open() -> Result<Self> {
        let monitor = Monitor::all()
            .context("Could not enumerate monitors")?
            .into_iter()
            .find(|monitor| monitor.is_primary().unwrap_or(false))
            .ok_or_else(|| anyhow!("No primary monitor found"))?;
        let source_size = (monitor.width()?, monitor.height()?);

        Ok(X11Source { monitor, source_size, frames: AtomicU64::new(0) })
    }
}

impl FrameSource for X11Source {
    fn next_frame(&self) -> Result<Frame> {
        let image = self.monitor.capture_image().context("Screen capture failed")?;
        let (width, height) = (image.width(), image.height());
        self.frames.fetch_add(1, Ordering::Relaxed);

        Frame::from_packed_rgba(image.into_raw(), width, height)
            .context("Screenshot was not a usable frame")
    }

    fn source_size(&self) -> (u32, u32) {
        self.source_size
    }

    fn frames_captured(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn describe(&self) -> String {
        "X11 screenshots".to_string()
    }
}

/// Shared between the GStreamer streaming thread and the sync loop.
#[derive(Default)]
struct StreamState {
    /// Frames are cheap to clone, so the sync loop takes one without copying
    /// pixels or holding the lock while it samples.
    frame: Option<Frame>,
    /// Learned from caps negotiated upstream of the scaler.
    source_size: Option<(u32, u32)>,
    /// The mode of the pipeline currently running, once it is up.
    mode: Option<CaptureMode>,
    /// Set when the pipeline thread dies, so startup fails instead of hanging.
    error: Option<String>,
}

/// A PipeWire screencast negotiated through the desktop portal.
struct WaylandSource {
    state: Arc<Mutex<StreamState>>,
    /// Counted outside the mutex so the streaming thread never blocks to report.
    frames: Arc<AtomicU64>,
    source_size: (u32, u32),
    mode: CaptureMode,
    /// Set on drop so the streaming thread tears its pipeline down.
    stopping: Arc<AtomicBool>,
    /// Holds the portal session open. Dropping it lets the compositor close the
    /// stream, so it must outlive the pipeline.
    _portal: PortalSession,
}

impl WaylandSource {
    fn open() -> Result<Self> {
        // A saved token is what makes a restart reuse the same monitor. Without
        // it the portal shows its source picker on every single start.
        let mut state_file = State::load();
        let (portal, token) = open_portal_session(state_file.portal_restore_token.clone())?;

        if token.is_some() && token != state_file.portal_restore_token {
            state_file.portal_restore_token = token;
            if let Err(e) = state_file.save() {
                debug!(error = ?e, "could not save the portal restore token");
            }
        }

        let frames = Arc::new(AtomicU64::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let state = start_stream(portal.node_id, Arc::clone(&frames), Arc::clone(&stopping))?;
        let (source_size, mode) = await_first_frame(&state)?;

        Ok(WaylandSource {
            state,
            frames,
            source_size,
            mode,
            stopping,
            _portal: portal,
        })
    }
}

impl FrameSource for WaylandSource {
    fn next_frame(&self) -> Result<Frame> {
        let guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("Capture state lock poisoned"))?;

        if let Some(error) = &guard.error {
            bail!("Screen capture pipeline failed: {error}");
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
        self.mode.describe().to_string()
    }
}

impl Drop for WaylandSource {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
    }
}

/// A negotiated portal session, kept alive for as long as this value exists.
struct PortalSession {
    node_id: u32,
    /// The session lives on its own thread, parked on this channel. Dropping the
    /// sender is what tells that thread to let the session go.
    _keepalive: mpsc::Sender<()>,
}

/// Negotiates a screencast portal session and returns its PipeWire node id,
/// along with a restore token to skip the picker next time if one was issued.
///
/// The session is deliberately parked on its own thread: the portal ties a
/// session to the D-Bus connection that created it, so tearing down the runtime
/// here would let the compositor close the stream out from under us.
fn open_portal_session(restore_token: Option<String>) -> Result<(PortalSession, Option<String>)> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let (keepalive_tx, keepalive_rx) = mpsc::channel::<()>();

    thread::Builder::new()
        .name("portal".into())
        .spawn(move || {
            let runtime = match Runtime::new() {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("Failed to start portal runtime: {e}")));
                    return;
                }
            };

            runtime.block_on(async move {
                let negotiated = negotiate(restore_token.as_deref()).await;

                match negotiated {
                    Ok((node_id, token, _proxy, _session)) => {
                        if ready_tx.send(Ok((node_id, token))).is_err() {
                            return;
                        }
                        // Hold the session open until the source is dropped. The
                        // blocking wait keeps the proxy and session alive inside
                        // the runtime that created them.
                        let _ = tokio::task::spawn_blocking(move || keepalive_rx.recv()).await;
                        debug!("closing the screencast portal session");
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            });
        })
        .context("Failed to start the portal thread")?;

    let (node_id, token) = ready_rx
        .recv_timeout(PORTAL_TIMEOUT)
        .context("Timed out waiting for the screen capture portal")??;

    Ok((PortalSession { node_id, _keepalive: keepalive_tx }, token))
}

/// The portal conversation itself. The proxy and session are returned so the
/// caller can keep them alive; dropping either ends the stream.
async fn negotiate(
    restore_token: Option<&str>,
) -> Result<(
    u32,
    Option<String>,
    Screencast<'static>,
    ashpd::desktop::Session<'static, Screencast<'static>>,
)> {
    let proxy = Screencast::new()
        .await
        .context("Could not reach the screencast portal")?;
    let session = proxy
        .create_session()
        .await
        .context("Portal refused to create a session")?;

    // Persistent so a restore token is issued; without one the source picker
    // appears on every start.
    proxy
        .select_sources(
            &session,
            CursorMode::Metadata,
            SourceType::Monitor | SourceType::Window,
            false,
            restore_token,
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .context("Source selection failed")?;

    let streams = proxy
        .start(&session, None)
        .await
        .context("Portal refused to start the stream")?
        .response()
        .context("Screen selection was cancelled")?;

    let node_id = streams
        .streams()
        .first()
        .map(|stream| stream.pipe_wire_node_id())
        .ok_or_else(|| anyhow!("Portal returned no streams"))?;
    let token = streams.restore_token().map(str::to_string);

    Ok((node_id, token, proxy, session))
}

/// Spawns the GStreamer pipeline on its own thread, recording any failure into
/// the shared state so startup can report it rather than spinning forever.
///
/// Capture modes are tried in order. A mode that fails before delivering a single
/// frame is treated as unsupported here and the next one is tried; a failure
/// after frames have flowed is a genuine runtime error and is reported.
fn start_stream(
    pipewire_id: u32,
    frames: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
) -> Result<Arc<Mutex<StreamState>>> {
    let state = Arc::new(Mutex::new(StreamState::default()));
    let thread_state = Arc::clone(&state);

    // ZYNC_FORCE_SHM exists so the fullscreen freeze can be reproduced on demand
    // when comparing modes. It is not a supported configuration.
    let force_shm = std::env::var("ZYNC_FORCE_SHM").is_ok_and(|value| value != "0");
    let modes: &[CaptureMode] = if force_shm {
        &[CaptureMode::SharedMemory]
    } else {
        &[CaptureMode::DmaBufGpu, CaptureMode::SharedMemory]
    };

    thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            let mut last_error = None;

            for &mode in modes {
                match run_pipeline(pipewire_id, mode, &thread_state, &frames, &stopping) {
                    Ok(()) => return,
                    Err(e) if frames.load(Ordering::Relaxed) > 0 => {
                        last_error = Some(e);
                        break;
                    }
                    Err(e) => {
                        info!(?mode, error = %format!("{e:#}"), "capture mode unavailable");
                        last_error = Some(e);
                    }
                }
            }

            if let (Some(e), Ok(mut guard)) = (last_error, thread_state.lock()) {
                guard.error = Some(format!("{e:#}"));
            }
        })
        .context("Failed to start the capture thread")?;

    Ok(state)
}

fn run_pipeline(
    pipewire_id: u32,
    mode: CaptureMode,
    state: &Arc<Mutex<StreamState>>,
    frames: &Arc<AtomicU64>,
    stopping: &Arc<AtomicBool>,
) -> Result<()> {
    gst::init().context("Failed to initialise GStreamer")?;

    let pipeline = gst::Pipeline::builder().name("zync-capture").build();

    let src = gst::ElementFactory::make("pipewiresrc")
        .property("path", pipewire_id.to_string())
        .build()
        .context("Failed to create pipewiresrc; is gst-plugin-pipewire installed?")?;

    // What we accept here is what pipewiresrc asks the compositor for. Only
    // offering memory:DMABuf makes it request a format with a DRM modifier,
    // which is the exact condition Mutter uses to decide whether to allocate
    // DMA-BUFs — and therefore whether it will record scanned-out frames.
    let source_filter = gst::ElementFactory::make("capsfilter")
        .build()
        .context("Failed to create source capsfilter")?;
    let max_framerate = gst::Fraction::new(CAPTURE_MAX_FPS, 1);
    let source_caps = match mode {
        CaptureMode::DmaBufGpu => gst::Caps::builder("video/x-raw")
            .features(["memory:DMABuf"])
            .field("max-framerate", max_framerate)
            .build(),
        CaptureMode::SharedMemory => gst::Caps::builder("video/x-raw")
            .field("max-framerate", max_framerate)
            .build(),
    };
    source_filter.set_property("caps", &source_caps);

    // Leaky so the compositor is never blocked waiting on us. Only the newest
    // frame matters, and stale ones are exactly what causes visible lag.
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 1u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .build()
        .context("Failed to create queue")?;
    queue.set_property_from_str("leaky", "downstream");

    // Drop surplus frames here, before scaling and conversion are paid for.
    // drop-only is set explicitly: without it videorate back-fills timestamp
    // gaps with duplicates, which after a stall (fullscreen app, sleep) means a
    // burst of hundreds of identical frames arriving at once.
    let videorate = gst::ElementFactory::make("videorate")
        .property("max-rate", CAPTURE_MAX_FPS)
        .property("drop-only", true)
        .property("skip-to-first", true)
        .build()
        .context("Failed to create videorate")?;

    let scaler = match mode {
        CaptureMode::DmaBufGpu => build_gpu_scaler()?,
        CaptureMode::SharedMemory => build_cpu_scaler()?,
    };

    let appsink = gst::ElementFactory::make("appsink")
        .name("sink")
        .build()
        .context("Failed to create appsink")?
        .downcast::<AppSink>()
        .map_err(|_| anyhow!("appsink element was not an AppSink"))?;

    appsink.set_max_buffers(1);
    appsink.set_drop(true);
    // Hand over the newest frame immediately instead of pacing to the pipeline
    // clock. Clock-syncing a live capture only adds latency for our purposes.
    appsink.set_property("sync", false);

    let mut elements: Vec<&gst::Element> = vec![&src, &source_filter, &queue, &videorate];
    elements.extend(scaler.iter());
    elements.push(appsink.upcast_ref());

    pipeline
        .add_many(&elements)
        .context("Failed to assemble capture pipeline")?;
    gst::Element::link_many(&elements).context("Failed to link capture pipeline")?;

    probe_source_size(&queue, state)?;
    attach_frame_callback(&appsink, state, Arc::clone(frames));

    if let Ok(mut guard) = state.lock() {
        guard.mode = Some(mode);
    }

    pipeline
        .set_state(gst::State::Playing)
        .context("Unable to start the capture pipeline")?;

    let result = watch_pipeline(&pipeline, mode, frames, stopping);
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Imports DMA-BUFs into GL and scales there, so the full-resolution frame never
/// touches system memory. Only the small result is downloaded.
fn build_gpu_scaler() -> Result<Vec<gst::Element>> {
    let missing =
        |name: &str| format!("Failed to create {name}; are the GStreamer OpenGL plugins installed?");

    let upload = gst::ElementFactory::make("glupload")
        .build()
        .with_context(|| missing("glupload"))?;
    let convert = gst::ElementFactory::make("glcolorconvert")
        .build()
        .with_context(|| missing("glcolorconvert"))?;
    let scale = gst::ElementFactory::make("glcolorscale")
        .build()
        .with_context(|| missing("glcolorscale"))?;

    let gl_filter = gst::ElementFactory::make("capsfilter")
        .build()
        .context("Failed to create GL capsfilter")?;
    gl_filter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .features(["memory:GLMemory"])
            .field("format", "RGBA")
            .field("width", CAPTURE_WIDTH)
            .field("height", CAPTURE_HEIGHT)
            .build(),
    );

    let download = gst::ElementFactory::make("gldownload")
        .build()
        .with_context(|| missing("gldownload"))?;

    let out_filter = gst::ElementFactory::make("capsfilter")
        .build()
        .context("Failed to create output capsfilter")?;
    out_filter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw").field("format", "RGBA").build(),
    );

    Ok(vec![upload, convert, scale, gl_filter, download, out_filter])
}

/// CPU conversion and scaling of shared-memory frames.
fn build_cpu_scaler() -> Result<Vec<gst::Element>> {
    // add-borders=false because letterbox bars would be averaged into the zone
    // colours. Stretching is harmless: zones are scaled per-axis independently,
    // so a zone still covers the same fraction of the screen either way.
    let converter = gst::ElementFactory::make("videoconvertscale")
        .property("add-borders", false)
        .build()
        .context("Failed to create videoconvertscale")?;

    let capsfilter = gst::ElementFactory::make("capsfilter")
        .build()
        .context("Failed to create capsfilter")?;

    // Framerate is intentionally left out of the caps: pipewiresrc commonly
    // advertises a variable rate, which makes an exact framerate negotiation
    // fail outright. videorate's max-rate already bounds it.
    capsfilter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", CAPTURE_WIDTH)
            .field("height", CAPTURE_HEIGHT)
            .build(),
    );

    Ok(vec![converter, capsfilter])
}

/// Keeps the streaming thread alive and surfaces pipeline errors, which a bare
/// GLib main loop would swallow silently. Also judges whether a mode is viable: a
/// mode that produces neither a frame nor an error within the negotiation window
/// is abandoned rather than waited on forever.
fn watch_pipeline(
    pipeline: &gst::Pipeline,
    mode: CaptureMode,
    frames: &Arc<AtomicU64>,
    stopping: &Arc<AtomicBool>,
) -> Result<()> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| anyhow!("Capture pipeline has no bus"))?;
    let started = Instant::now();

    loop {
        if stopping.load(Ordering::Relaxed) {
            debug!("capture pipeline shutting down");
            return Ok(());
        }

        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    bail!(
                        "{} ({})",
                        err.error(),
                        err.debug().unwrap_or_else(|| "no detail".into())
                    );
                }
                gst::MessageView::Eos(_) => bail!("Capture stream ended unexpectedly"),
                _ => {}
            }
        }

        if frames.load(Ordering::Relaxed) == 0 && started.elapsed() > MODE_NEGOTIATION_TIMEOUT {
            bail!("{mode:?} produced no frames within {MODE_NEGOTIATION_TIMEOUT:?}");
        }
    }
}

/// Reads the display's native resolution off the caps upstream of the scaler.
/// This is what lets zone coordinates stay in the user's own screen space.
fn probe_source_size(element: &gst::Element, state: &Arc<Mutex<StreamState>>) -> Result<()> {
    let sink_pad = element
        .static_pad("sink")
        .ok_or_else(|| anyhow!("Probe target has no sink pad"))?;
    let probe_state = Arc::clone(state);

    sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data
            && let gst::EventView::Caps(caps_event) = event.view()
            && let Some(structure) = caps_event.caps().structure(0)
            && let (Ok(width), Ok(height)) =
                (structure.get::<i32>("width"), structure.get::<i32>("height"))
            && let Ok(mut guard) = probe_state.lock()
        {
            guard.source_size = Some((width as u32, height as u32));
        }

        gst::PadProbeReturn::Ok
    });

    Ok(())
}

fn attach_frame_callback(
    appsink: &AppSink,
    state: &Arc<Mutex<StreamState>>,
    frames: Arc<AtomicU64>,
) {
    let sink_state = Arc::clone(state);

    appsink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let caps = sample.caps().ok_or(gst::FlowError::Error)?;

                // Go through VideoFrameRef rather than a raw map: the GL download
                // path attaches a GstVideoMeta whose stride is the only reliable
                // source of row pitch, and the buffer may be larger than the image.
                let info = gst_video::VideoInfo::from_caps(caps).map_err(|_| gst::FlowError::Error)?;
                let video_frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
                    .map_err(|_| gst::FlowError::Error)?;
                let plane = video_frame.plane_data(0).map_err(|_| gst::FlowError::Error)?;
                let stride = video_frame.plane_stride()[0] as usize;

                // The mapping is only valid inside this callback, so the pixels
                // are copied out once. Padding is copied along with them and
                // described by the stride rather than being stripped here.
                let frame = match Frame::new(Arc::from(plane), info.width(), info.height(), stride) {
                    Ok(frame) => frame,
                    Err(e) => {
                        warn!(error = ?e, "discarding an unusable frame");
                        return Ok(gst::FlowSuccess::Ok);
                    }
                };

                let mut guard = sink_state.lock().map_err(|_| gst::FlowError::Error)?;
                guard.frame = Some(frame);
                frames.fetch_add(1, Ordering::Relaxed);

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
}

/// Blocks until the pipeline yields a frame, failing fast if the streaming thread
/// reported an error rather than spinning indefinitely.
fn await_first_frame(state: &Arc<Mutex<StreamState>>) -> Result<((u32, u32), CaptureMode)> {
    let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;

    loop {
        {
            let guard = state
                .lock()
                .map_err(|_| anyhow!("Capture state lock poisoned"))?;

            if let Some(error) = &guard.error {
                bail!("Screen capture pipeline failed: {error}");
            }

            if let (Some(_), Some(size), Some(mode)) =
                (&guard.frame, guard.source_size, guard.mode)
            {
                return Ok((size, mode));
            }
        }

        if Instant::now() >= deadline {
            bail!("Timed out waiting for the first frame from the compositor");
        }

        thread::sleep(Duration::from_millis(20));
    }
}
