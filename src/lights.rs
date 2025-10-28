#![allow(dead_code)]


use rumqttc::{Client, QoS};
use serde_json::json;
use anyhow::Result;
use serde::Deserialize;

use crate::capture::ZoneSample;


//this is used to format the payload for various services. HueAPI isn't zigbee but including it as I have plans to make it in scope as the application adds different connection types beyond MQTT
//
#[derive(Deserialize)]
pub enum LightService {
    Zigbee2MQTT,
    ZHA,
    HueAPI
}

#[derive(Deserialize)]
pub struct LightConfig {
    pub name: String,
    pub service: LightService,
    pub device_name: String,
    pub brightness: u8,
}

pub struct ColorCommand {r: u8, g: u8, b: u8}

impl ColorCommand {
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl From<ZoneSample> for ColorCommand {
    fn from(sample: ZoneSample) -> Self {
        Self::new(sample.r, sample.g, sample.b)
    }
}

pub struct LightController {config: LightConfig, client: Client}

impl LightController {
    pub fn new(config: LightConfig, client: Client) -> Self {
        LightController { config, client }
    }

    pub fn get_topic (&self) -> String {
        match self.config.service {
            LightService::Zigbee2MQTT => format!("zigbee2mqtt/{}/set", self.config.device_name),
            LightService::ZHA => format!("zigbee2mqtt/{}/set", self.config.device_name), //placeholder for now - just Z2M
            LightService::HueAPI => format!("zigbee2mqtt/{}/set", self.config.device_name), //placeholder for now - just Z2M
        }
    }

    pub fn format_payload(&self, color: ColorCommand, transition: f32) -> Vec<u8>{
        let payload = json!({
            "color": {
                "r": color.r,
                "g": color.g,
                "b": color.b
            },
            "brightness": self.config.brightness,
            "transition": transition
            });

        payload.to_string().into_bytes()
    }

    pub fn set_light(&mut self, color: ColorCommand, transition: Option<f32>) -> Result<()> {
        let t = transition.unwrap_or(0.0);
        let light = self.get_topic();
        let payload = self.format_payload(color, t);

        self.client.try_publish(light, QoS::AtMostOnce, false, payload)?;
        Ok(())
    }
}
