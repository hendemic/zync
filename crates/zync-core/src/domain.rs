//! Pure model: frames, colours, zone geometry, and the configuration shape.
//!
//! No I/O and no platform dependencies. Everything here is directly testable.

use serde::Deserialize;
use std::fmt;
use std::sync::Arc;
use thiserror::Error;

/// Below this brightness a sample is blended toward a warm white. Fully dark
/// scenes otherwise drive lights to an unpleasant near-black colour cast.
const BRIGHTNESS_COLOR_THRESH: u8 = 25;
const MIN_BRIGHTNESS: u8 = 1;
/// Exponent applied to normalised luminance. Below 1.0 it lifts midtones, which
/// tracks perceived brightness better than the linear value.
const BRIGHTNESS_CURVE: f32 = 0.7;

/// Zigbee group commands travel as broadcasts, and a mesh sustains roughly one
/// broadcast per second before its transaction tables fill and everything after
/// that is refused or queued for seconds. Unicast to a single device has no such
/// ceiling, though bulbs still process commands serially.
const GROUP_UPDATES_PER_SEC: f32 = 1.0;
const DEVICE_UPDATES_PER_SEC: f32 = 4.0;

/// Largest possible distance between two RGB triples, i.e. black to white.
const MAX_COLOR_DISTANCE: f32 = 441.0;

const BYTES_PER_PIXEL: usize = 4;

/// A frame a capture backend handed over that cannot be read as an image.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame has a zero dimension ({width}x{height})")]
    ZeroSized { width: u32, height: u32 },
    #[error("frame stride {stride} is narrower than one {width}px row")]
    StrideTooNarrow { stride: usize, width: u32 },
    #[error(
        "frame buffer holds {actual} bytes, need {required} for {width}x{height} at stride {stride}"
    )]
    Truncated {
        actual: usize,
        required: usize,
        width: u32,
        height: u32,
        stride: usize,
    },
    #[error("frame dimensions {width}x{height} at stride {stride} overflow")]
    Overflow { width: u32, height: u32, stride: usize },
}

/// A zone that cannot be sampled, either because it is malformed or because the
/// frame it was asked to read does not line up with it.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ZoneError {
    #[error("capture source reported a zero-sized display")]
    ZeroSizedSource,
    #[error("zone '{zone}' has a zero dimension")]
    ZeroSizedZone { zone: String },
    #[error("zone '{zone}' sampled no pixels; check its coordinates")]
    NoPixelsSampled { zone: String },
}

/// A configuration that parsed but cannot be run.
#[derive(Debug, Error, PartialEq)]
pub enum ConfigError {
    #[error("no lights configured")]
    NoLights,
    #[error("no zones configured")]
    NoZones,
    #[error("light '{light}' has brightness {brightness}; expected 0.0 to 1.0")]
    BrightnessOutOfRange { light: LightId, brightness: f32 },
    #[error("zone '{zone}' references unknown light '{light}'")]
    UnknownLight { zone: String, light: LightId },
    #[error("custom intensity curve has cut_midpoint {midpoint}; expected 0.0 to 1.0")]
    CurveMidpointOutOfRange { midpoint: f32 },
    #[error("custom intensity curve has cut_steepness {steepness}; expected a value greater than 0.0")]
    CurveSteepnessNotPositive { steepness: f32 },
    #[error("custom intensity curve has min_transition {min}; expected a non-negative value")]
    CurveMinTransitionNegative { min: f32 },
    #[error("custom intensity curve has min_transition {min} greater than max_transition {max}")]
    CurveMinExceedsMax { min: f32, max: f32 },
}

/// A captured frame in RGBA8, borrowed rather than copied.
///
/// `stride` is carried explicitly because capture backends pad rows to alignment
/// boundaries. Honouring it here means the padding never has to be stripped by
/// copying the whole frame first.
#[derive(Clone)]
pub struct Frame {
    data: Arc<[u8]>,
    width: u32,
    height: u32,
    stride: usize,
}

impl Frame {
    pub fn new(
        data: Arc<[u8]>,
        width: u32,
        height: u32,
        stride: usize,
    ) -> Result<Self, FrameError> {
        if width == 0 || height == 0 {
            return Err(FrameError::ZeroSized { width, height });
        }

        let overflow = || FrameError::Overflow { width, height, stride };
        let row_bytes = (width as usize)
            .checked_mul(BYTES_PER_PIXEL)
            .ok_or_else(overflow)?;
        if stride < row_bytes {
            return Err(FrameError::StrideTooNarrow { stride, width });
        }

        // The final row needs only its pixels, not a full stride of padding.
        let required = stride
            .checked_mul(height as usize - 1)
            .and_then(|full| full.checked_add(row_bytes))
            .ok_or_else(overflow)?;
        if data.len() < required {
            return Err(FrameError::Truncated {
                actual: data.len(),
                required,
                width,
                height,
                stride,
            });
        }

        Ok(Frame { data, width, height, stride })
    }

    /// Convenience for backends that hand over tightly packed RGBA.
    pub fn from_packed_rgba(data: Vec<u8>, width: u32, height: u32) -> Result<Self, FrameError> {
        let stride = (width as usize).saturating_mul(BYTES_PER_PIXEL);
        Frame::new(data.into(), width, height, stride)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn rgb(&self, x: u32, y: u32) -> Option<Rgb> {
        if x >= self.width || y >= self.height {
            return None;
        }

        let offset = y as usize * self.stride + x as usize * BYTES_PER_PIXEL;
        let pixel = self.data.get(offset..offset + 3)?;
        Some(Rgb::new(pixel[0], pixel[1], pixel[2]))
    }
}

/// A colour, whether sampled from the screen or destined for a light.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// Euclidean distance in RGB space. Not perceptually uniform, but the
    /// thresholds downstream are tuned against it.
    pub fn distance(&self, other: &Rgb) -> f32 {
        let dr = self.r as f32 - other.r as f32;
        let dg = self.g as f32 - other.g as f32;
        let db = self.b as f32 - other.b as f32;

        (dr * dr + dg * dg + db * db).sqrt()
    }

    pub fn differs_from(&self, other: &Rgb, threshold: u8) -> bool {
        self.distance(other) > threshold as f32
    }
}

/// Shapes how transition time falls off with colour distance.
///
/// `base(d)` is the old soft falloff: below 1.0, `softness` makes even small
/// jumps reasonably quick, reserving the long fades for near-identical colours
/// where a slow blend reads as smooth rather than sluggish. `gate` rides on top
/// of that as a sigmoid centred on `cut_midpoint`: it stays near 1 (leaving
/// `base` alone) for gradual changes and collapses toward 0 (pulling the result
/// down to `min_transition`) once the distance passes the midpoint, so a cut or
/// explosion fades out fast without changing the pacing of small changes at all.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub struct TransitionCurve {
    /// Exponent applied to normalised distance in the base falloff.
    pub softness: f32,
    /// Normalised distance (0..=1) where the cut gate is half open.
    pub cut_midpoint: f32,
    /// How sharply the cut gate closes around `cut_midpoint`. Higher is snappier.
    pub cut_steepness: f32,
    /// Floor on transition time, in seconds.
    pub min_transition: f32,
    /// Ceiling on transition time, in seconds; also the fallback for a first
    /// sample that has nothing to transition from.
    pub max_transition: f32,
}

impl TransitionCurve {
    /// Fade time for moving between two samples: long for near-identical colours
    /// so gradual scenes read as smooth, short for big jumps so cuts and
    /// explosions stay snappy.
    pub fn transition(&self, from: &Rgb, to: &Rgb) -> f32 {
        let normalized = (from.distance(to) / MAX_COLOR_DISTANCE).min(1.0);

        let base = self.max_transition
            - normalized.powf(self.softness) * (self.max_transition - self.min_transition);
        let gate = 1.0 / (1.0 + (self.cut_steepness * (normalized - self.cut_midpoint)).exp());

        base * gate + self.min_transition * (1.0 - gate)
    }
}

/// How aggressively big colour jumps are shortened, from a gentle "slow" fade
/// through the default to "extreme", which snaps almost instantly on a cut.
/// `Custom` takes a hand-tuned curve for anyone the presets don't fit.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Intensity {
    /// Gentle fades throughout; suits film and ambient content where even cuts
    /// should ease rather than snap.
    Slow,
    /// The default balance: quick on cuts, smooth on gradual changes.
    #[default]
    Normal,
    /// Snaps almost instantly on cuts and explosions; suits fast-paced games.
    Extreme,
    /// A hand-tuned curve, given as its five parameters directly.
    Custom(TransitionCurve),
}

impl Intensity {
    /// Resolves a preset (or a custom setting, unchanged) to the curve it drives.
    pub fn curve(&self) -> TransitionCurve {
        match self {
            Intensity::Slow => TransitionCurve {
                softness: 0.6,
                cut_midpoint: 0.6,
                cut_steepness: 10.0,
                min_transition: 0.10,
                max_transition: 1.5,
            },
            Intensity::Normal => TransitionCurve {
                softness: 0.4,
                cut_midpoint: 0.4,
                cut_steepness: 14.0,
                min_transition: 0.02,
                max_transition: 1.0,
            },
            Intensity::Extreme => TransitionCurve {
                softness: 0.3,
                cut_midpoint: 0.25,
                cut_steepness: 16.0,
                min_transition: 0.02,
                max_transition: 0.6,
            },
            Intensity::Custom(curve) => *curve,
        }
    }
}

/// What a light is asked to do. Derived from a sample, before any per-light
/// brightness scaling is applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightCommand {
    pub color: Rgb,
    pub brightness: u8,
    pub transition: f32,
}

impl LightCommand {
    /// Derives brightness from the sample's luminance, and warms the colour as it
    /// approaches black so dark scenes do not read as a muddy grey.
    pub fn from_sample(sample: Rgb, transition: f32) -> Self {
        let luminance =
            0.299 * sample.r as f32 + 0.587 * sample.g as f32 + 0.114 * sample.b as f32;
        let brightness = ((luminance / 255.0).powf(BRIGHTNESS_CURVE) * 255.0)
            .clamp(MIN_BRIGHTNESS as f32, 255.0) as u8;

        let color = if brightness < BRIGHTNESS_COLOR_THRESH {
            let warmth = 1.0 - brightness as f32 / BRIGHTNESS_COLOR_THRESH as f32;
            let blend = |channel: u8, target: f32| {
                (channel as f32 * (1.0 - warmth) + target * warmth) as u8
            };
            Rgb::new(
                blend(sample.r, 250.0),
                blend(sample.g, 210.0),
                blend(sample.b, 190.0),
            )
        } else {
            sample
        };

        LightCommand { color, brightness, transition }
    }

    /// Applies a light's configured brightness ceiling, giving the values it will
    /// actually receive.
    pub fn scaled(self, factor: f32) -> Self {
        let brightness = (factor * self.brightness as f32).clamp(0.0, 255.0) as u8;
        LightCommand { brightness, ..self }
    }
}

/// A light's name in whatever service owns it. Used as the key linking zones,
/// pacing state, and delivery failures to one another.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
pub struct LightId(String);

impl LightId {
    pub fn new(name: impl Into<String>) -> Self {
        LightId(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LightId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which service owns a light, and therefore how its commands are addressed.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub enum LightService {
    Zigbee2MQTT,
    ZHA,
    HueAPI,
}

#[derive(Clone, Debug, Deserialize)]
pub struct LightSpec {
    pub service: LightService,
    pub light_name: LightId,
    pub brightness: f32,
    /// Whether `light_name` is a group rather than a single device. Groups are
    /// paced far more conservatively because their commands are broadcast.
    #[serde(default)]
    pub is_group: bool,
    /// Overrides the pacing default chosen from `is_group`.
    #[serde(default)]
    pub max_updates_per_sec: Option<f32>,
    /// State to leave this light in when a session ends and its previous state
    /// could not be read back. Passed through to the service verbatim, so any
    /// payload the service accepts works here.
    #[serde(default)]
    pub fallback_state: Option<serde_json::Value>,
}

impl LightSpec {
    pub fn updates_per_sec(&self) -> f32 {
        self.max_updates_per_sec.unwrap_or(if self.is_group {
            GROUP_UPDATES_PER_SEC
        } else {
            DEVICE_UPDATES_PER_SEC
        })
    }
}

/// A rectangular region of the screen, in native display pixels, and the light
/// that follows it.
#[derive(Clone, Debug, Deserialize)]
pub struct Zone {
    pub name: String,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub light_name: LightId,
}

/// Averages a zone's colour out of a frame.
///
/// Zones are declared at the display's native resolution, but frames arrive
/// scaled down for performance. Bridging those two coordinate spaces is this
/// type's whole job.
pub struct ZoneSampler {
    zone: Zone,
    source_size: (u32, u32),
}

impl ZoneSampler {
    pub fn new(zone: Zone, source_size: (u32, u32)) -> Result<Self, ZoneError> {
        let (width, height) = source_size;
        if width == 0 || height == 0 {
            return Err(ZoneError::ZeroSizedSource);
        }
        if zone.width == 0 || zone.height == 0 {
            return Err(ZoneError::ZeroSizedZone { zone: zone.name });
        }

        Ok(ZoneSampler { zone, source_size })
    }

    pub fn light(&self) -> &LightId {
        &self.zone.light_name
    }

    pub fn name(&self) -> &str {
        &self.zone.name
    }

    /// Average colour of the zone. `downsample` is a stride in native display
    /// pixels, scaled to match the frame so a value tuned on a 4K screen keeps
    /// its meaning on a smaller one.
    pub fn sample(&self, frame: &Frame, downsample: u8) -> Result<Rgb, ZoneError> {
        let (frame_width, frame_height) = frame.size();
        let (source_width, source_height) = self.source_size;
        let scale_x = frame_width as f64 / source_width as f64;
        let scale_y = frame_height as f64 / source_height as f64;

        let to_frame =
            |value: u32, scale: f64, limit: u32| ((value as f64 * scale) as u32).min(limit);

        let x_start = to_frame(self.zone.x, scale_x, frame_width - 1);
        let y_start = to_frame(self.zone.y, scale_y, frame_height - 1);

        // Far edges are pushed out by at least one pixel so thin zones, or zones
        // on a heavily scaled frame, never collapse to nothing.
        let x_end = to_frame(self.zone.x.saturating_add(self.zone.width), scale_x, frame_width)
            .max(x_start + 1);
        let y_end = to_frame(self.zone.y.saturating_add(self.zone.height), scale_y, frame_height)
            .max(y_start + 1);

        let step_x = ((downsample.max(1) as f64 * scale_x).round() as usize).max(1);
        let step_y = ((downsample.max(1) as f64 * scale_y).round() as usize).max(1);

        let (mut r_sum, mut g_sum, mut b_sum, mut count) = (0u64, 0u64, 0u64, 0u64);
        for y in (y_start..y_end).step_by(step_y) {
            for x in (x_start..x_end).step_by(step_x) {
                if let Some(pixel) = frame.rgb(x, y) {
                    r_sum += pixel.r as u64;
                    g_sum += pixel.g as u64;
                    b_sum += pixel.b as u64;
                    count += 1;
                }
            }
        }

        if count == 0 {
            return Err(ZoneError::NoPixelsSampled { zone: self.zone.name.clone() });
        }

        Ok(Rgb::new(
            (r_sum / count) as u8,
            (g_sum / count) as u8,
            (b_sum / count) as u8,
        ))
    }
}

/// What to do with the lights when a session ends.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopPolicy {
    /// Put each light back the way it was before the session started, falling
    /// back to `fallback_state` for any light whose state could not be read.
    #[default]
    Restore,
    /// Always apply `fallback_state`, ignoring whatever was read at startup.
    Default,
    /// Turn every light off.
    Off,
    /// Leave the lights showing the last colour they were sent.
    Hold,
}

#[derive(Clone, Debug, Deserialize)]
pub struct MqttConfig {
    pub name: String,
    pub broker: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
}

fn default_max_commands_per_sec() -> f32 {
    6.0
}

#[derive(Clone, Debug, Deserialize)]
pub struct PerformanceConfig {
    pub max_fps: u64,
    pub max_delay: u64,
    pub refresh_threshold: u8,
    pub percent_thread_work: f32,
    pub fps_reporting: u64,
    /// Ceiling on light commands per second across every zone. Configs written
    /// before this field existed keep loading, hence the default.
    #[serde(default = "default_max_commands_per_sec")]
    pub max_commands_per_sec: f32,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub mqtt: MqttConfig,
    pub lights: Vec<LightSpec>,
    pub zones: Vec<Zone>,
    pub downsample_factor: u8,
    pub performance: PerformanceConfig,
    /// What to leave the lights doing when the session stops.
    #[serde(default)]
    pub on_stop: StopPolicy,
    /// Names this installation on the broker, keeping its client id and control
    /// topics distinct from another machine's.
    ///
    /// Left unset here on purpose: resolving the default means asking the system
    /// for its hostname, which is an adapter's job, not the model's.
    #[serde(default)]
    pub instance: Option<String>,
    /// How aggressively big colour jumps are shortened. Defaults to a balanced
    /// preset so configs written before this field existed keep loading.
    #[serde(default)]
    pub intensity: Intensity,
}

impl Config {
    /// Rejects configurations that would fail later, at a point where the cause
    /// is harder to report. This is what `zync config validate` will run.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.lights.is_empty() {
            return Err(ConfigError::NoLights);
        }
        if self.zones.is_empty() {
            return Err(ConfigError::NoZones);
        }

        if let Some(light) = self
            .lights
            .iter()
            .find(|light| !(0.0..=1.0).contains(&light.brightness))
        {
            return Err(ConfigError::BrightnessOutOfRange {
                light: light.light_name.clone(),
                brightness: light.brightness,
            });
        }

        if let Some(zone) = self
            .zones
            .iter()
            .find(|zone| self.light(&zone.light_name).is_none())
        {
            return Err(ConfigError::UnknownLight {
                zone: zone.name.clone(),
                light: zone.light_name.clone(),
            });
        }

        if let Intensity::Custom(curve) = self.intensity {
            if !(0.0..=1.0).contains(&curve.cut_midpoint) {
                return Err(ConfigError::CurveMidpointOutOfRange { midpoint: curve.cut_midpoint });
            }
            if curve.cut_steepness <= 0.0 {
                return Err(ConfigError::CurveSteepnessNotPositive {
                    steepness: curve.cut_steepness,
                });
            }
            if curve.min_transition < 0.0 {
                return Err(ConfigError::CurveMinTransitionNegative { min: curve.min_transition });
            }
            if curve.min_transition > curve.max_transition {
                return Err(ConfigError::CurveMinExceedsMax {
                    min: curve.min_transition,
                    max: curve.max_transition,
                });
            }
        }

        Ok(())
    }

    pub fn light(&self, id: &LightId) -> Option<&LightSpec> {
        self.lights.iter().find(|light| &light.light_name == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(x: u32, y: u32, width: u32, height: u32) -> Zone {
        Zone {
            name: "test".to_string(),
            x,
            y,
            width,
            height,
            light_name: LightId::new("test_light"),
        }
    }

    /// Left half red, right half blue, tightly packed.
    fn split_frame(width: u32, height: u32) -> Frame {
        let mut data = Vec::with_capacity((width * height) as usize * BYTES_PER_PIXEL);
        for _ in 0..height {
            for x in 0..width {
                let pixel = if x < width / 2 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 255, 255]
                };
                data.extend_from_slice(&pixel);
            }
        }
        Frame::from_packed_rgba(data, width, height).unwrap()
    }

    #[test]
    fn native_zone_maps_onto_a_scaled_frame() {
        // Left half of a 4K display, sampled from a 640x360 frame.
        let sampler = ZoneSampler::new(zone(0, 0, 1920, 2160), (3840, 2160)).unwrap();

        assert_eq!(sampler.sample(&split_frame(640, 360), 1).unwrap(), Rgb::new(255, 0, 0));
    }

    #[test]
    fn right_half_zone_reads_the_right_half() {
        let sampler = ZoneSampler::new(zone(1920, 0, 1920, 2160), (3840, 2160)).unwrap();

        assert_eq!(sampler.sample(&split_frame(640, 360), 1).unwrap(), Rgb::new(0, 0, 255));
    }

    #[test]
    fn unscaled_frame_leaves_coordinates_untouched() {
        let sampler = ZoneSampler::new(zone(0, 0, 50, 100), (100, 100)).unwrap();

        assert_eq!(sampler.sample(&split_frame(100, 100), 1).unwrap(), Rgb::new(255, 0, 0));
    }

    #[test]
    fn full_screen_zone_stays_within_frame_bounds() {
        let sampler = ZoneSampler::new(zone(0, 0, 3840, 2160), (3840, 2160)).unwrap();
        let sample = sampler.sample(&split_frame(640, 360), 1).unwrap();

        // Half red, half blue averages to a dark purple rather than erroring on
        // an out-of-bounds read at the far edge.
        assert!(sample.r > 100 && sample.b > 100);
    }

    #[test]
    fn zone_thinner_than_one_scaled_pixel_still_samples() {
        // A 4px strip on a 4K screen is 0.6px once scaled to a 360px-tall frame.
        let sampler = ZoneSampler::new(zone(0, 2156, 3840, 4), (3840, 2160)).unwrap();

        assert!(sampler.sample(&split_frame(640, 360), 20).is_ok());
    }

    #[test]
    fn zero_sized_source_is_rejected() {
        assert!(ZoneSampler::new(zone(0, 0, 100, 100), (0, 0)).is_err());
    }

    #[test]
    fn zero_sized_zone_is_rejected() {
        assert!(ZoneSampler::new(zone(0, 0, 0, 100), (100, 100)).is_err());
    }

    #[test]
    fn padded_rows_are_read_using_stride() {
        // Two 2px rows at a 3px stride: red row, blue row, one pixel of padding.
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        let pad = [7u8, 7, 7, 7];

        let mut data = Vec::new();
        data.extend_from_slice(&red);
        data.extend_from_slice(&red);
        data.extend_from_slice(&pad);
        data.extend_from_slice(&blue);
        data.extend_from_slice(&blue);

        let frame = Frame::new(data.into(), 2, 2, 3 * BYTES_PER_PIXEL).unwrap();

        assert_eq!(frame.rgb(1, 0).unwrap(), Rgb::new(255, 0, 0), "first row should be red");
        assert_eq!(frame.rgb(1, 1).unwrap(), Rgb::new(0, 0, 255), "second row should be blue");
    }

    #[test]
    fn tightly_packed_frames_need_no_padding_allowance() {
        let frame = split_frame(2, 1);

        assert_eq!(frame.rgb(0, 0).unwrap(), Rgb::new(255, 0, 0));
        assert_eq!(frame.rgb(1, 0).unwrap(), Rgb::new(0, 0, 255));
    }

    #[test]
    fn stride_narrower_than_a_row_is_rejected() {
        assert!(Frame::new(vec![0; 64].into(), 4, 4, 3 * BYTES_PER_PIXEL).is_err());
    }

    #[test]
    fn truncated_buffers_are_rejected() {
        assert!(Frame::from_packed_rgba(vec![0; 15], 2, 2).is_err());
    }

    /// The last row carries no trailing padding, so requiring a full stride for
    /// it would reject frames the capture backends legitimately produce.
    #[test]
    fn final_row_may_omit_its_padding() {
        let stride = 3 * BYTES_PER_PIXEL;
        let data = vec![0u8; stride + 2 * BYTES_PER_PIXEL];

        assert!(Frame::new(data.into(), 2, 2, stride).is_ok());
    }

    #[test]
    fn reads_outside_the_frame_return_nothing() {
        let frame = split_frame(4, 4);

        assert!(frame.rgb(4, 0).is_none());
        assert!(frame.rgb(0, 4).is_none());
    }

    #[test]
    fn dark_samples_are_warmed_rather_than_left_grey() {
        let command = LightCommand::from_sample(Rgb::new(4, 4, 4), 1.0);

        assert!(command.brightness < BRIGHTNESS_COLOR_THRESH);
        assert!(
            command.color.r > command.color.b,
            "expected a warm cast, got {:?}",
            command.color
        );
    }

    #[test]
    fn bright_samples_keep_their_colour() {
        let sample = Rgb::new(200, 100, 50);

        assert_eq!(LightCommand::from_sample(sample, 1.0).color, sample);
    }

    #[test]
    fn brightness_never_reaches_zero() {
        assert!(LightCommand::from_sample(Rgb::new(0, 0, 0), 1.0).brightness >= MIN_BRIGHTNESS);
    }

    #[test]
    fn scaling_reduces_brightness_but_not_colour() {
        let command = LightCommand::from_sample(Rgb::new(200, 100, 50), 1.0);
        let scaled = command.scaled(0.5);

        assert_eq!(scaled.color, command.color);
        assert_eq!(scaled.brightness, command.brightness / 2);
    }

    #[test]
    fn big_colour_jumps_transition_faster_than_small_ones() {
        let curve = Intensity::Normal.curve();
        let black = Rgb::new(0, 0, 0);
        let near = Rgb::new(4, 4, 4);
        let white = Rgb::new(255, 255, 255);

        assert!(curve.transition(&black, &white) < curve.transition(&black, &near));
        // Black to white is the maximum distance, so it lands on the floor;
        // compare with a tolerance rather than exactly.
        assert!(curve.transition(&black, &white) >= curve.min_transition - 1e-6);
        assert!(curve.transition(&black, &black) <= curve.max_transition);
    }

    /// The old formula, before the cut gate was layered on top, kept for the
    /// small-distance regression test below.
    fn old_curve_transition(distance: f32) -> f32 {
        const SOFTNESS: f32 = 0.4;
        const MIN: f32 = 0.02;
        const MAX: f32 = 1.0;

        let normalized = (distance / MAX_COLOR_DISTANCE).min(1.0);
        MAX - normalized.powf(SOFTNESS) * (MAX - MIN)
    }

    #[test]
    fn normal_preset_matches_the_old_curve_for_small_distances() {
        let curve = Intensity::Normal.curve();
        let black = Rgb::new(0, 0, 0);

        // d <= 0.2 of MAX_COLOR_DISTANCE, i.e. up to ~88 in RGB distance.
        for distance in [0.0f32, 20.0, 50.0, 88.0] {
            let to = Rgb::new(distance.min(255.0) as u8, 0, 0);
            let old = old_curve_transition(distance);
            let new = curve.transition(&black, &to);

            assert!(
                (old - new).abs() <= 0.03,
                "distance {distance}: old={old}, new={new}, diff={}",
                (old - new).abs()
            );
        }
    }

    #[test]
    fn normal_preset_is_much_faster_for_big_jumps() {
        let curve = Intensity::Normal.curve();
        let black = Rgb::new(0, 0, 0);
        let white = Rgb::new(255, 255, 255);

        assert!(
            (curve.transition(&black, &white) - curve.min_transition).abs() < 1e-3,
            "black to white should land on the floor"
        );

        // d = 0.5 of MAX_COLOR_DISTANCE; only the red channel differs from black,
        // so that channel alone carries the whole distance.
        let half = Rgb::new((MAX_COLOR_DISTANCE * 0.5) as u8, 0, 0);
        assert!(
            curve.transition(&black, &half) <= 0.10,
            "d=0.5 should fade in 0.10s or less, got {}",
            curve.transition(&black, &half)
        );
    }

    /// Samples a curve's transition time across the full distance range, as
    /// approximated by a single reddening channel from black.
    fn sample_curve(curve: TransitionCurve) -> Vec<f32> {
        let black = Rgb::new(0, 0, 0);
        (0..=100)
            .map(|step| {
                let d = step as f32 / 100.0;
                let to = Rgb::new((d * 255.0) as u8, (d * 255.0) as u8, (d * 255.0) as u8);
                curve.transition(&black, &to)
            })
            .collect()
    }

    #[test]
    fn every_preset_is_monotonically_non_increasing_with_distance() {
        for intensity in [Intensity::Slow, Intensity::Normal, Intensity::Extreme] {
            let samples = sample_curve(intensity.curve());
            assert!(
                samples.windows(2).all(|pair| pair[0] >= pair[1] - 1e-4),
                "{intensity:?} transition time should never increase with distance"
            );
        }
    }

    #[test]
    fn slow_is_never_faster_than_normal_which_is_never_faster_than_extreme() {
        let slow = sample_curve(Intensity::Slow.curve());
        let normal = sample_curve(Intensity::Normal.curve());
        let extreme = sample_curve(Intensity::Extreme.curve());

        for i in 0..slow.len() {
            assert!(slow[i] >= normal[i] - 1e-4, "slow[{i}]={} normal[{i}]={}", slow[i], normal[i]);
            assert!(
                normal[i] >= extreme[i] - 1e-4,
                "normal[{i}]={} extreme[{i}]={}",
                normal[i],
                extreme[i]
            );
        }
    }

    #[test]
    fn intensity_extreme_parses_from_a_bare_string() {
        let intensity: Intensity = serde_json::from_str(r#""extreme""#).unwrap();
        assert_eq!(intensity, Intensity::Extreme);
    }

    #[test]
    fn intensity_custom_parses_from_a_table() {
        let json = r#"{
            "custom": {
                "softness": 0.5,
                "cut_midpoint": 0.5,
                "cut_steepness": 12.0,
                "min_transition": 0.05,
                "max_transition": 1.2
            }
        }"#;
        let intensity: Intensity = serde_json::from_str(json).unwrap();

        assert_eq!(
            intensity,
            Intensity::Custom(TransitionCurve {
                softness: 0.5,
                cut_midpoint: 0.5,
                cut_steepness: 12.0,
                min_transition: 0.05,
                max_transition: 1.2,
            })
        );
    }

    #[test]
    fn missing_intensity_defaults_to_normal() {
        let config = config_with(vec![light("a", 0.8)], vec![zone_for("a")]);

        assert_eq!(config.intensity, Intensity::Normal);
    }

    #[test]
    fn custom_curve_with_min_above_max_fails_validation() {
        let mut config = config_with(vec![light("a", 0.8)], vec![zone_for("a")]);
        config.intensity = Intensity::Custom(TransitionCurve {
            softness: 0.4,
            cut_midpoint: 0.4,
            cut_steepness: 14.0,
            min_transition: 2.0,
            max_transition: 1.0,
        });

        assert!(config.validate().is_err());
    }

    #[test]
    fn groups_are_paced_slower_than_devices() {
        let spec = |is_group| LightSpec {
            service: LightService::Zigbee2MQTT,
            light_name: LightId::new("l"),
            brightness: 1.0,
            is_group,
            max_updates_per_sec: None,
            fallback_state: None,
        };

        assert!(spec(true).updates_per_sec() < spec(false).updates_per_sec());
    }

    fn config_with(lights: Vec<LightSpec>, zones: Vec<Zone>) -> Config {
        Config {
            mqtt: MqttConfig {
                name: "test".into(),
                broker: "localhost".into(),
                port: 1883,
                user: None,
                password: None,
            },
            lights,
            zones,
            downsample_factor: 20,
            performance: PerformanceConfig {
                max_fps: 12,
                max_delay: 500,
                refresh_threshold: 10,
                percent_thread_work: 0.25,
                fps_reporting: 10,
                max_commands_per_sec: 6.0,
            },
            on_stop: StopPolicy::Restore,
            instance: None,
            intensity: Intensity::Normal,
        }
    }

    fn light(name: &str, brightness: f32) -> LightSpec {
        LightSpec {
            service: LightService::Zigbee2MQTT,
            light_name: LightId::new(name),
            brightness,
            is_group: false,
            max_updates_per_sec: None,
            fallback_state: None,
        }
    }

    fn zone_for(light_name: &str) -> Zone {
        Zone {
            name: "z".into(),
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            light_name: LightId::new(light_name),
        }
    }

    #[test]
    fn a_zone_pointing_at_no_light_fails_validation() {
        let config = config_with(vec![light("a", 1.0)], vec![zone_for("b")]);

        assert!(config.validate().is_err());
    }

    #[test]
    fn matching_lights_and_zones_validate() {
        let config = config_with(vec![light("a", 0.8)], vec![zone_for("a")]);

        assert!(config.validate().is_ok());
    }

    #[test]
    fn out_of_range_brightness_fails_validation() {
        let config = config_with(vec![light("a", 1.5)], vec![zone_for("a")]);

        assert!(config.validate().is_err());
    }
}
