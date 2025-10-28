#![allow(dead_code)]


use rumqttc::{Client, QoS};
use serde_json::json;
use anyhow::Result;


//this is used to format the payload for various services. HueAPI isn't zigbee but including it as I have plans to make it in scope as the application adds different connection types beyond MQTT
pub enum LightService {
    Zigbee2MQTT,
    ZHA,
    ESPHome,
    HueAPI
}

pub struct LightConfig {
    name: String,
    service: LightService,
    device_name: String,
    max_brightness: u8,
}

pub struct ColorCommand {
    r: u8,
    g: u8,
    b: u8,
    brightness: u8,
}

impl ColorCommand {
    pub fn new(r: u8, g: u8, b: u8, brightness: u8) -> Self {
        Self { r, g, b, brightness }
    }
    //In the future add a From method to directly use the ScreenSample. Leaving this method out until we have a working capture module to make testing easier with light commands.
    // impl From<ScreenSample> for ColorCommand {
    //     fn from(sample: ScreenSample) -> Self {
    //         Self::new(sample.r, sample.g, sample.b, sample.brightness)
    //     }
    // }
}

pub struct LightController {
    config: LightConfig,
    client: Client,
}

impl LightController {
    pub fn new(config: LightConfig, client: Client) -> Self {
        LightController { config, client }
    }

    pub fn get_topic (&self) -> String {
        match self.config.service {
            LightService::Zigbee2MQTT => format!("zigbee2mqtt/{}/set", self.config.device_name),
            LightService::ZHA => format!("zigbee2mqtt/{}/set", self.config.device_name), //placeholder for now - just Z2M
            LightService::ESPHome => format!("zigbee2mqtt/{}/set", self.config.device_name), //placeholder for now - just Z2M
            LightService::HueAPI => format!("zigbee2mqtt/{}/set", self.config.device_name), //placeholder for now - just Z2M
        }
    }

    pub fn format_payload(&self, color: ColorCommand) -> Vec<u8>{
        let topic = json!({
            "color": {
                "r": color.r,
                "g": color.g,
                "b": color.b
            },
            "brightness": color.brightness
            });

        topic.to_string().into_bytes()
    }

    pub fn set_light(&mut self, color: ColorCommand) -> Result<()> {
        let light = self.get_topic();
        let payload = self.format_payload(color);

        self.client.try_publish(light, QoS::AtMostOnce, false, payload)?;
        Ok(())
    }
}
