#![allow(dead_code, unused_imports, unused_variables)]

use rumqttc::{Client, QoS};
use serde_json::json;
use anyhow::{Result, Context};
use serde::Deserialize;

use crate::capture::ZoneColor;


const BRIGHTNESS_COLOR_THRESH: u8 = 25;
const MIN_BRIGHTNESS: u8 = 1;
const BRIGHTNESS_EXP: f32 = 1.3;
const BRIGHTNESS_FACTOR: f32 = 1.1;

//this is used to format the payload for various services. HueAPI isn't zigbee but including it as I have plans to make it in scope as the application adds different connection types beyond MQTT
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
}

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

pub struct LightController <'a> {config: LightConfig, client: &'a Client}

impl<'a> LightController<'a> {
    pub fn new(config: LightConfig, client: &'a Client) -> Self {
        LightController { config, client }
    }
    pub fn get_light_name (&self) -> String {
        self.config.light_name.clone()
    }
    fn get_topic (&self) -> String {
        match self.config.service {
            LightService::Zigbee2MQTT => format!("zigbee2mqtt/{}/set", self.config.light_name),
            LightService::ZHA => format!("zigbee2mqtt/{}/set", self.config.light_name), //placeholder for now - just Z2M
            LightService::HueAPI => format!("zigbee2mqtt/{}/set", self.config.light_name), //placeholder for now - just Z2M
        }
    }

    fn format_payload(&self, color: MessageColor, transition: f32) -> Vec<u8>{
        let brightness: u8 = (self.config.brightness * color.brightness as f32).min(255.0) as u8;

        let payload = json!({
            "color": {
                "r": color.r,
                "g": color.g,
                "b": color.b
            },
            "brightness": brightness,
            "transition": transition
            });

        payload.to_string().into_bytes()
    }

    pub fn set_light(&self, color: MessageColor, transition: Option<f32>) -> Result<()> {
        let t = transition.unwrap_or(0.0);
        let light = self.get_topic();
        let payload = self.format_payload(color, t);

        self.client.try_publish(&light, QoS::AtMostOnce, false, payload)
            .with_context(|| format!("Failed to publish to topic {}", light))?;
        Ok(())
    }
}
