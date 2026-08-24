use anyhow::{anyhow, bail, Context, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app::{AppSink, AppSinkCallbacks};
use image::RgbaImage;
use serde::Deserialize;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use xcap::*;

/// Averaging a zone's colour needs almost no spatial detail, so frames are scaled
/// down inside the pipeline rather than hauling native-resolution buffers around.
/// A 4K frame costs 31.6 MiB; this costs 0.9 MiB. Zone configuration is unaffected
/// because zones are declared in native display coordinates and translated by
/// [`ZoneSampler::sample`].
const CAPTURE_WIDTH: i32 = 640;
const CAPTURE_HEIGHT: i32 = 360;

/// Frames arriving faster than this are dropped before any scaling or colour
/// conversion is paid for. Well above the rate lights can physically follow.
const CAPTURE_MAX_FPS: i32 = 15;

/// Portal negotiation blocks on the user picking a monitor, so this is generous.
const PORTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to wait for the compositor to deliver a first frame once streaming.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// Captures screen across platforms
pub trait ScreenCapture {
    fn new() -> Result<Box<dyn ScreenCapture>>
    where
        Self: Sized;

    /// The most recent frame. May be scaled down relative to [`Self::source_size`].
    fn capture_frame(&self) -> Result<Arc<RgbaImage>>;

    /// Native resolution of the capture source. Zone configuration is written in
    /// this coordinate space no matter what resolution frames actually arrive at.
    fn source_size(&self) -> (u32, u32);

    fn stop(&mut self) -> Result<()>; //unused, but keeping in interface as a reminder that thread is created in new()
}

//Structs for X11, Wayland, and in the future MacOS and Windows.
pub struct X11Capturer {
    monitor: Monitor,
    source_size: (u32, u32),
}

pub struct WaylandCapturer {
    state: Arc<Mutex<StreamState>>,
    source_size: (u32, u32),
}

/// Shared between the GStreamer streaming thread and the sync loop.
#[derive(Default)]
struct StreamState {
    /// Behind an `Arc` so the sync loop can take the frame without copying pixels.
    frame: Option<Arc<RgbaImage>>,
    /// Learned from caps negotiated upstream of the scaler.
    source_size: Option<(u32, u32)>,
    /// Set when the pipeline thread dies, so startup fails instead of hanging.
    error: Option<String>,
}

impl ScreenCapture for X11Capturer {
    fn new() -> Result<Box<dyn ScreenCapture>> {
        let monitor = Monitor::all()?
            .into_iter()
            .find(|m| m.is_primary().unwrap_or(false))
            .ok_or_else(|| anyhow!("No primary monitor found"))?;

        let source_size = (monitor.width()?, monitor.height()?);

        Ok(Box::new(X11Capturer {
            monitor,
            source_size,
        }))
    }

    fn capture_frame(&self) -> Result<Arc<RgbaImage>> {
        Ok(Arc::new(self.monitor.capture_image()?))
    }

    fn source_size(&self) -> (u32, u32) {
        self.source_size
    }

    fn stop(&mut self) -> Result<()> {
        println!("Nothing stopped - no stream to end on X11");
        Ok(())
    }
}

impl ScreenCapture for WaylandCapturer {
    fn new() -> Result<Box<dyn ScreenCapture>> {
        let pipewire_id = Self::open_portal_session()?;
        let state = Self::start_stream(pipewire_id)?;
        let source_size = Self::await_first_frame(&state)?;

        Ok(Box::new(WaylandCapturer { state, source_size }))
    }

    fn capture_frame(&self) -> Result<Arc<RgbaImage>> {
        let guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("Capture state lock poisoned"))?;

        if let Some(err) = &guard.error {
            bail!("Screen capture pipeline failed: {err}");
        }

        guard
            .frame
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| anyhow!("No frame available"))
    }

    fn source_size(&self) -> (u32, u32) {
        self.source_size
    }

    // This is a placeholder. Process will run for the entiretly of the program's lifecycle
    // If I add any CLI or HomeAssistant trigger to start and stop syncing, I may need to revisit this
    // Leaving this in the interface as a reminder this may be necessary, and that starting this bg thread from within the object on new() is a little messy.
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

impl WaylandCapturer {
    /// Negotiates a screencast portal session and returns its PipeWire node id.
    ///
    /// The session is deliberately parked on its own thread and never dropped: the
    /// portal ties a session to the D-Bus connection that created it, so tearing
    /// down the runtime here would let the compositor close the stream out from
    /// under us.
    fn open_portal_session() -> Result<u32> {
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let runtime = match Runtime::new() {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = tx.send(Err(anyhow!("Failed to start portal runtime: {e}")));
                    return;
                }
            };

            runtime.block_on(async move {
                let negotiated = async {
                    let proxy = Screencast::new()
                        .await
                        .context("Could not reach the screencast portal")?;
                    let session = proxy
                        .create_session()
                        .await
                        .context("Portal refused to create a session")?;

                    //prompt user to select monitor
                    proxy
                        .select_sources(
                            &session,
                            CursorMode::Metadata,
                            (SourceType::Monitor | SourceType::Window).into(),
                            false,
                            None,
                            PersistMode::ExplicitlyRevoked,
                        )
                        .await
                        .context("Source selection failed")?;

                    let node_id = proxy
                        .start(&session, None)
                        .await
                        .context("Portal refused to start the stream")?
                        .response()
                        .context("Screen selection was cancelled")?
                        .streams()
                        .first()
                        .map(|stream| stream.pipe_wire_node_id())
                        .ok_or_else(|| anyhow!("Portal returned no streams"))?;

                    Ok::<_, anyhow::Error>((node_id, proxy, session))
                }
                .await;

                match negotiated {
                    Ok((node_id, _proxy, _session)) => {
                        if tx.send(Ok(node_id)).is_err() {
                            return;
                        }
                        // Hold the session open for the lifetime of the process.
                        std::future::pending::<()>().await;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            });
        });

        rx.recv_timeout(PORTAL_TIMEOUT)
            .context("Timed out waiting for the screen capture portal")?
    }

    /// Spawns the GStreamer pipeline on its own thread, recording any failure into
    /// the shared state so startup can report it rather than spinning forever.
    fn start_stream(pipewire_id: u32) -> Result<Arc<Mutex<StreamState>>> {
        let state = Arc::new(Mutex::new(StreamState::default()));
        let thread_state = Arc::clone(&state);

        thread::spawn(move || {
            if let Err(e) = Self::run_pipeline(pipewire_id, &thread_state) {
                if let Ok(mut guard) = thread_state.lock() {
                    guard.error = Some(format!("{e:#}"));
                }
            }
        });

        Ok(state)
    }

    fn run_pipeline(pipewire_id: u32, state: &Arc<Mutex<StreamState>>) -> Result<()> {
        gst::init().context("Failed to initialise GStreamer")?;

        let pipeline = gst::Pipeline::builder().name("zync-capture").build();

        let src = gst::ElementFactory::make("pipewiresrc")
            .property("path", pipewire_id.to_string())
            .build()
            .context("Failed to create pipewiresrc; is gst-plugin-pipewire installed?")?;

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
        // `max-rate` implies drop-only, so no frames are ever duplicated.
        let videorate = gst::ElementFactory::make("videorate")
            .property("max-rate", CAPTURE_MAX_FPS)
            .build()
            .context("Failed to create videorate")?;

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
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", CAPTURE_WIDTH)
            .field("height", CAPTURE_HEIGHT)
            .build();
        capsfilter.set_property("caps", &caps);

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

        let elements = [
            &src,
            &queue,
            &videorate,
            &converter,
            &capsfilter,
            appsink.upcast_ref(),
        ];
        pipeline
            .add_many(elements)
            .context("Failed to assemble capture pipeline")?;
        gst::Element::link_many(elements).context("Failed to link capture pipeline")?;

        Self::probe_source_size(&converter, state)?;
        Self::attach_frame_callback(&appsink, state);

        pipeline
            .set_state(gst::State::Playing)
            .context("Unable to start the capture pipeline")?;

        // Keeps this thread alive and surfaces pipeline errors, which the previous
        // bare GLib main loop would have swallowed silently.
        let bus = pipeline
            .bus()
            .ok_or_else(|| anyhow!("Capture pipeline has no bus"))?;

        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    bail!(
                        "{} ({})",
                        err.error(),
                        err.debug().unwrap_or_else(|| "no detail".into())
                    );
                }
                gst::MessageView::Eos(_) => {
                    let _ = pipeline.set_state(gst::State::Null);
                    bail!("Capture stream ended unexpectedly");
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Reads the display's native resolution off the caps upstream of the scaler.
    /// This is what lets zone coordinates stay in the user's own screen space.
    fn probe_source_size(converter: &gst::Element, state: &Arc<Mutex<StreamState>>) -> Result<()> {
        let sink_pad = converter
            .static_pad("sink")
            .ok_or_else(|| anyhow!("Scaler has no sink pad"))?;

        let probe_state = Arc::clone(state);

        sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                if let gst::EventView::Caps(caps_event) = event.view() {
                    if let Some(structure) = caps_event.caps().structure(0) {
                        if let (Ok(width), Ok(height)) = (
                            structure.get::<i32>("width"),
                            structure.get::<i32>("height"),
                        ) {
                            if let Ok(mut guard) = probe_state.lock() {
                                guard.source_size = Some((width as u32, height as u32));
                            }
                        }
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });

        Ok(())
    }

    fn attach_frame_callback(appsink: &AppSink, state: &Arc<Mutex<StreamState>>) {
        let sink_state = Arc::clone(state);

        appsink.set_callbacks(
            AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;

                    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
                    let structure = caps.structure(0).ok_or(gst::FlowError::Error)?;
                    let width = structure
                        .get::<i32>("width")
                        .map_err(|_| gst::FlowError::Error)? as u32;
                    let height = structure
                        .get::<i32>("height")
                        .map_err(|_| gst::FlowError::Error)? as u32;

                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                    if let Some(img) = pack_frame(map.as_slice(), width, height) {
                        let mut guard = sink_state.lock().map_err(|_| gst::FlowError::Error)?;
                        guard.frame = Some(Arc::new(img));
                    }

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    /// Blocks until the pipeline yields a frame, failing fast if the streaming
    /// thread reported an error rather than spinning indefinitely.
    fn await_first_frame(state: &Arc<Mutex<StreamState>>) -> Result<(u32, u32)> {
        let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;

        loop {
            {
                let guard = state
                    .lock()
                    .map_err(|_| anyhow!("Capture state lock poisoned"))?;

                if let Some(err) = &guard.error {
                    bail!("Screen capture pipeline failed: {err}");
                }

                if let (Some(_), Some(size)) = (&guard.frame, guard.source_size) {
                    return Ok(size);
                }
            }

            if Instant::now() >= deadline {
                bail!("Timed out waiting for the first frame from the compositor");
            }

            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Copies a mapped GStreamer buffer into an `RgbaImage`, honouring row stride.
///
/// GStreamer pads rows to alignment boundaries, so treating the mapping as tightly
/// packed skews the image whenever stride exceeds `width * 4`.
fn pack_frame(data: &[u8], width: u32, height: u32) -> Option<RgbaImage> {
    if width == 0 || height == 0 {
        return None;
    }

    let row_bytes = (width as usize).checked_mul(4)?;
    let expected = row_bytes.checked_mul(height as usize)?;

    if data.len() == expected {
        return RgbaImage::from_raw(width, height, data.to_vec());
    }

    let stride = data.len() / height as usize;
    if stride < row_bytes {
        return None;
    }

    let mut packed = Vec::with_capacity(expected);
    for row in 0..height as usize {
        let start = row * stride;
        packed.extend_from_slice(data.get(start..start + row_bytes)?);
    }

    RgbaImage::from_raw(width, height, packed)
}

/// Constructor for new ScreenCapture based on platform
pub fn new_screen() -> Result<Box<dyn ScreenCapture>> {
    #[cfg(target_os = "linux")]
    {
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            //TODO need to check if there are other checks to make sure I accurately detect wayland
            WaylandCapturer::new()
        } else {
            X11Capturer::new()
        }
    }
    #[cfg(target_os = "windows")]
    bail!("Windows not yet supported");

    #[cfg(target_os = "macos")]
    bail!("MacOS not yet supported");
}

/// rectangular zone on screen to sample color from, in native display pixels
#[derive(Deserialize)]
pub struct ZoneConfig {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    light_name: String,
}

/// This is a color sample from the screen. Its separate from ColorCommand because it implements differs_from and both could have their own unique functions in the future.
#[derive(Clone, Copy)]
pub struct ZoneColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl ZoneColor {
    /// constructor for new ZoneColor
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        ZoneColor { r, g, b }
    }
    pub fn compare_sample(&self, other: &ZoneColor) -> f32 {
        let dr = (self.r as f32 - other.r as f32).abs();
        let dg = (self.g as f32 - other.g as f32).abs();
        let db = (self.b as f32 - other.b as f32).abs();

        (dr.powi(2) + dg.powi(2) + db.powi(2)).sqrt()
    }
    ///this function checks if any color channel exceeds a given threshold
    pub fn differs_from(&self, other: &ZoneColor, threshold: u8) -> bool {
        let diff = self.compare_sample(other);
        diff > threshold as f32
    }
}

///Used to sample a region on a monitor
pub struct ZoneSampler {
    config: ZoneConfig,
    /// Resolution the zone rectangle is expressed in — the user's real display.
    /// Frames may arrive smaller than this; `sample` bridges the two.
    source_size: (u32, u32),
}

impl ZoneSampler {
    pub fn new(config: ZoneConfig, source_size: (u32, u32)) -> Result<Self> {
        let (width, height) = source_size;
        if width == 0 || height == 0 {
            bail!("Capture source reported a zero-sized display");
        }

        Ok(ZoneSampler {
            config,
            source_size,
        })
    }

    pub fn get_light_name(&self) -> String {
        self.config.light_name.clone()
    }

    /// Captures average rgb values for a zone.
    ///
    /// Zones are configured in native display coordinates, but frames are scaled
    /// down for performance, so the rectangle is translated into frame space here.
    /// `downsample` is likewise treated as a native-pixel stride and scaled to
    /// match, so a value tuned against a 4K screen keeps its meaning.
    pub fn sample(&self, screenshot: &RgbaImage, downsample: u8) -> Result<ZoneColor> {
        let (frame_width, frame_height) = (screenshot.width(), screenshot.height());
        if frame_width == 0 || frame_height == 0 {
            bail!("Received an empty frame");
        }

        let (source_width, source_height) = self.source_size;
        let scale_x = frame_width as f64 / source_width as f64;
        let scale_y = frame_height as f64 / source_height as f64;

        let to_frame = |value: u32, scale: f64, limit: u32| ((value as f64 * scale) as u32).min(limit);

        //set loop start + stop for iterating through pixels, clamped into the frame
        let x_start = to_frame(self.config.x, scale_x, frame_width - 1);
        let y_start = to_frame(self.config.y, scale_y, frame_height - 1);

        // Far edges are pushed out by one pixel minimum so thin zones, or zones on
        // a heavily scaled frame, never collapse to nothing.
        let x_end = to_frame(
            self.config.x.saturating_add(self.config.width),
            scale_x,
            frame_width,
        )
        .max(x_start + 1);
        let y_end = to_frame(
            self.config.y.saturating_add(self.config.height),
            scale_y,
            frame_height,
        )
        .max(y_start + 1);

        let step_x = ((downsample.max(1) as f64 * scale_x).round() as usize).max(1);
        let step_y = ((downsample.max(1) as f64 * scale_y).round() as usize).max(1);

        // Calculate average
        let mut r_sum = 0u64;
        let mut g_sum = 0u64;
        let mut b_sum = 0u64;
        let mut count = 0u64;

        for y_pixel in (y_start..y_end).step_by(step_y) {
            for x_pixel in (x_start..x_end).step_by(step_x) {
                let pixel = screenshot.get_pixel(x_pixel, y_pixel);
                r_sum += pixel[0] as u64;
                g_sum += pixel[1] as u64;
                b_sum += pixel[2] as u64;
                count += 1;
            }
        }

        if count == 0 {
            bail!(
                "Zone for '{}' sampled no pixels; check its coordinates",
                self.config.light_name
            );
        }

        Ok(ZoneColor {
            r: (r_sum / count) as u8,
            g: (g_sum / count) as u8,
            b: (b_sum / count) as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn zone(x: u32, y: u32, width: u32, height: u32) -> ZoneConfig {
        ZoneConfig {
            x,
            y,
            width,
            height,
            light_name: "test-light".to_string(),
        }
    }

    /// Frame whose left half is red and right half is blue.
    fn split_frame(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, _| {
            if x < width / 2 {
                Rgba([255, 0, 0, 255])
            } else {
                Rgba([0, 0, 255, 255])
            }
        })
    }

    #[test]
    fn native_zone_maps_onto_a_scaled_frame() {
        // Left half of a 4K display, sampled from a 640x360 capture.
        let sampler = ZoneSampler::new(zone(0, 0, 1920, 2160), (3840, 2160)).unwrap();
        let color = sampler.sample(&split_frame(640, 360), 1).unwrap();

        assert_eq!((color.r, color.g, color.b), (255, 0, 0));
    }

    #[test]
    fn right_half_zone_reads_the_right_half() {
        let sampler = ZoneSampler::new(zone(1920, 0, 1920, 2160), (3840, 2160)).unwrap();
        let color = sampler.sample(&split_frame(640, 360), 1).unwrap();

        assert_eq!((color.r, color.g, color.b), (0, 0, 255));
    }

    #[test]
    fn unscaled_frame_leaves_coordinates_untouched() {
        let sampler = ZoneSampler::new(zone(0, 0, 50, 100), (100, 100)).unwrap();
        let color = sampler.sample(&split_frame(100, 100), 1).unwrap();

        assert_eq!((color.r, color.g, color.b), (255, 0, 0));
    }

    #[test]
    fn full_screen_zone_stays_within_frame_bounds() {
        let sampler = ZoneSampler::new(zone(0, 0, 3840, 2160), (3840, 2160)).unwrap();

        assert!(sampler.sample(&split_frame(640, 360), 25).is_ok());
    }

    #[test]
    fn zone_thinner_than_one_scaled_pixel_still_samples() {
        // Four native rows collapse to well under a pixel at 640x360.
        let sampler = ZoneSampler::new(zone(0, 2156, 3840, 4), (3840, 2160)).unwrap();

        assert!(sampler.sample(&split_frame(640, 360), 25).is_ok());
    }

    #[test]
    fn zero_sized_source_is_rejected() {
        assert!(ZoneSampler::new(zone(0, 0, 100, 100), (0, 0)).is_err());
    }

    #[test]
    fn padded_rows_are_unpacked_using_stride() {
        // 2x2 RGBA carrying 4 bytes of row padding, as GStreamer may deliver.
        let (width, height, stride) = (2u32, 2u32, 12usize);
        let mut data = vec![0u8; stride * height as usize];
        data[0..8].copy_from_slice(&[255, 0, 0, 255, 255, 0, 0, 255]);
        data[12..20].copy_from_slice(&[0, 0, 255, 255, 0, 0, 255, 255]);

        let image = pack_frame(&data, width, height).expect("padded frame should unpack");

        assert_eq!(image.get_pixel(1, 0)[0], 255, "first row should be red");
        assert_eq!(image.get_pixel(1, 1)[2], 255, "second row should be blue");
    }

    #[test]
    fn tightly_packed_frames_are_unpacked_unchanged() {
        let data: Vec<u8> = [255, 0, 0, 255, 0, 0, 255, 255].to_vec();

        let image = pack_frame(&data, 2, 1).expect("packed frame should unpack");

        assert_eq!(image.get_pixel(0, 0)[0], 255);
        assert_eq!(image.get_pixel(1, 0)[2], 255);
    }
}
