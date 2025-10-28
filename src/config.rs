#![allow(dead_code)]
use rumqttc::{Client, Connection, MqttOptions};
use anyhow::Result;

use crate::lights::LightConfig;

pub struct AppConfig {
    pub mqtt: MQTTConfig,
    pub lights: Vec<LightConfig>,
    // pub zones: Vec<ZoneConfig>,
    // pub sampling: SamplingConfig,
    // pub performance: PerformanceConfig,
}


impl AppConfig {
    //loads the toml file to generate MQTTConfig, LightConfig vector, ZoneConfig vector, SamplingConfig, and PerformanceConfig while creating the AppConfig instance. These fields get passed into the app's various structs.
    pub fn load() -> Result<Self>{
        todo!("Build out toml structure and loader")
    }

}

pub struct MQTTConfig {
    pub name: String,
    pub broker: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
}


impl MQTTConfig {
    // This is a wrapper around rumqttc that creates the client and connection from the MQTTConfig thats created during app configuration (AppConfig.load)
    pub fn create_client(&self) -> Result<Client, Connection> {
        todo!("Build out mqtt client and connection creation")
    }
}
