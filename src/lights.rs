#![allow(dead_code, unused_imports, unused_variables)]

use rumqttc::{Client, QoS};
use serde_json::json;
use anyhow::{Result, Context};
use serde::Deserialize;

use crate::capture::ZoneSample;


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

pub struct LightController <'a> {config: LightConfig, client: &'a Client}

impl<'a> LightController<'a> {
    pub fn new(config: LightConfig, client: &'a Client) -> Self {
        LightController { config, client }
    }

    pub fn get_topic (&self) -> String {
        match self.config.service {
            LightService::Zigbee2MQTT => format!("zigbee2mqtt/{}/set", self.config.light_name),
            LightService::ZHA => format!("zigbee2mqtt/{}/set", self.config.light_name), //placeholder for now - just Z2M
            LightService::HueAPI => format!("zigbee2mqtt/{}/set", self.config.light_name), //placeholder for now - just Z2M
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

        println!("Topic: {t}", t = &light);
        self.client.try_publish(light, QoS::AtLeastOnce, false, payload)
            .context("Failed to send light message")?;

        Ok(())
    }
}
