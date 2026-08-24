#![allow(dead_code, unused_imports, unused_variables)]

use rumqttc::{Client, QoS};
use serde_json::{json, Map, Value};
use anyhow::{Result, Context};
use serde::Deserialize;

use crate::capture::ZoneColor;


const BRIGHTNESS_COLOR_THRESH: u8 = 25;
const MIN_BRIGHTNESS: u8 = 1;
const BRIGHTNESS_EXP: f32 = 1.3;
const BRIGHTNESS_FACTOR: f32 = 1.1;

/// Zigbee group commands travel as broadcasts, and a mesh sustains roughly one
/// broadcast per second before its transaction tables fill and everything after
/// that is refused or queued for seconds. Unicast to a single device has no such
/// ceiling, though bulbs still process commands serially.
const GROUP_UPDATES_PER_SEC: f32 = 1.0;
const DEVICE_UPDATES_PER_SEC: f32 = 4.0;

/// Below these deltas a field is left out of the payload. Zigbee2MQTT turns each
/// field into its own ZCL command, so resending an unchanged brightness costs a
/// whole extra network transaction for nothing visible.
const COLOR_RESEND_DELTA: u8 = 2;
const BRIGHTNESS_RESEND_DELTA: u8 = 4;

//this is used to format the payload for various services. HueAPI isn't zigbee but including it as I am interested in making it in scope as the application adds different connection types beyond MQTT
#[derive(Deserialize, Debug, Clone)]
pub enum LightService {
    Zigbee2MQTT,
    ZHA,
    HueAPI
}

#[derive(Deserialize, Debug, Clone)]
pub struct LightConfig {
    pub service: LightService,
    pub light_name: String,
    pub brightness: f32,
    /// Whether `light_name` is a Zigbee2MQTT group rather than a single device.
    /// Groups are paced far more conservatively because their commands are
    /// broadcast. Defaults to false so existing configs keep loading.
    #[serde(default)]
    pub is_group: bool,
    /// Overrides the pacing default chosen from `is_group`.
    #[serde(default)]
    pub max_updates_per_sec: Option<f32>,
}

impl LightConfig {
    pub fn updates_per_sec(&self) -> f32 {
        self.max_updates_per_sec.unwrap_or(if self.is_group {
            GROUP_UPDATES_PER_SEC
        } else {
            DEVICE_UPDATES_PER_SEC
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageColor {r: u8, g: u8, b: u8, brightness: u8}

impl MessageColor {
    pub fn new(r: u8, g: u8, b: u8, brightness: u8) -> Self {
        Self { r, g, b, brightness }
    }
}

impl From<ZoneColor> for MessageColor {
    fn from(sample: ZoneColor) -> Self {
        let brightness = (0.299 * sample.r as f32) + (0.587  * sample.g as f32) + (0.114 * sample.b as f32);

        let normalized = brightness / 255.0;
        let amplified = normalized.powf(0.7);

        let final_brightness = (amplified * 255.0).min(255.0).max(MIN_BRIGHTNESS as f32) as u8;

        let (r, g, b) = if final_brightness < BRIGHTNESS_COLOR_THRESH {
            let mix_factor = 1.0 - (final_brightness as f32 / BRIGHTNESS_COLOR_THRESH as f32);

            let r = (sample.r as f32 * (1.0 - mix_factor) + 250.0 * mix_factor) as u8;
            let g = (sample.g as f32 * (1.0 - mix_factor) + 210.0 * mix_factor) as u8;
            let b = (sample.b as f32 * (1.0 - mix_factor) + 190.0 * mix_factor) as u8;

            (r, g, b)
        } else {
           (sample.r, sample.g, sample.b)
        };

        Self::new(r, g, b, final_brightness)
    }
}

pub struct LightController <'a> {
    config: LightConfig,
    client: &'a Client,
    /// What the light was last told, after brightness scaling. Tracked per field
    /// so a skipped field is still compared against the value the light holds,
    /// and small drifts accumulate into a send rather than being lost forever.
    last_sent: Option<MessageColor>,
}

impl<'a> LightController<'a> {
    pub fn new(config: LightConfig, client: &'a Client) -> Self {
        LightController { config, client, last_sent: None }
    }
    pub fn get_light_name (&self) -> String {
        self.config.light_name.clone()
    }
    pub fn updates_per_sec(&self) -> f32 {
        self.config.updates_per_sec()
    }
    fn get_topic (&self) -> String {
        match self.config.service {
            LightService::Zigbee2MQTT => format!("zigbee2mqtt/{}/set", self.config.light_name),
            LightService::ZHA => format!("zigbee2mqtt/{}/set", self.config.light_name), //placeholder for now - just Z2M
            LightService::HueAPI => format!("zigbee2mqtt/{}/set", self.config.light_name), //placeholder for now - just Z2M
        }
    }

    /// Applies the configured brightness scaling, giving the values the light
    /// will actually receive.
    fn scaled(&self, color: MessageColor) -> MessageColor {
        let brightness = (self.config.brightness * color.brightness as f32).min(255.0) as u8;
        MessageColor { brightness, ..color }
    }

    /// Which fields differ enough from the last command to be worth a transaction.
    fn changes(&self, next: MessageColor) -> (bool, bool) {
        match self.last_sent {
            None => (true, true),
            Some(prev) => {
                let color = [(prev.r, next.r), (prev.g, next.g), (prev.b, next.b)]
                    .iter()
                    .any(|(before, after)| before.abs_diff(*after) >= COLOR_RESEND_DELTA);
                let brightness = prev.brightness.abs_diff(next.brightness) >= BRIGHTNESS_RESEND_DELTA;
                (color, brightness)
            }
        }
    }

    /// Whether `set_light` would send anything. Checked before spending budget,
    /// since a change visible in the sample can still round to the same command.
    pub fn needs_update(&self, color: MessageColor) -> bool {
        let (send_color, send_brightness) = self.changes(self.scaled(color));
        send_color || send_brightness
    }

    /// Sends only the fields that changed. Returns whether anything was sent.
    pub fn set_light(&mut self, color: MessageColor, transition: Option<f32>) -> Result<bool> {
        let next = self.scaled(color);
        let (send_color, send_brightness) = self.changes(next);

        if !send_color && !send_brightness {
            return Ok(false);
        }

        let mut payload = Map::new();
        if send_color {
            payload.insert("color".into(), json!({ "r": next.r, "g": next.g, "b": next.b }));
        }
        if send_brightness {
            payload.insert("brightness".into(), json!(next.brightness));
        }
        payload.insert("transition".into(), json!(transition.unwrap_or(0.0)));

        let topic = self.get_topic();
        let body = Value::Object(payload).to_string().into_bytes();

        self.client.try_publish(&topic, QoS::AtMostOnce, false, body)
            .with_context(|| format!("Failed to publish to topic {}", topic))?;

        let mut recorded = self.last_sent.unwrap_or(next);
        if send_color {
            recorded.r = next.r;
            recorded.g = next.g;
            recorded.b = next.b;
        }
        if send_brightness {
            recorded.brightness = next.brightness;
        }
        self.last_sent = Some(recorded);

        Ok(true)
    }
}
