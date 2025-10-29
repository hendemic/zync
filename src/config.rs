#![allow(dead_code, unused_imports, unused_variables)]

use rumqttc::*;
use serde::Deserialize;
use anyhow::{Result, Context, bail};
use std::fs;
use dirs;
use std::time::Duration;

use crate::lights::LightConfig;
use crate::capture::ZoneConfig;
use crate::sync::PerformanceConfig;


// App config loads all of the configuratoin parameters for the app, including mqtt configs, the lights, zones, and global settings for the app.
// These are passed into the other objects with config.field_name syntax.
// The thinking here is I wanted the structs to be defined in their module, but I wanted the app config to all be centralized.
#[derive(Deserialize)]
pub struct AppConfig {
    pub mqtt: MQTTConfig,
    pub lights: Vec<LightConfig>,
    pub zones: Vec<ZoneConfig>,
    downsample_factor: u8,
    performance: PerformanceConfig,
}

impl AppConfig {
    /// loads the yaml file using dir for cross-platform compatibility, and serde_yaml to construct the AppConfig
    pub fn load() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .context("Could not find config directory")?
            .join("zync");
        let path = config_dir.join("config.yaml");

        if !path.exists() {
            if std::env::var("USER").unwrap_or_default() == "root" {
                bail!("Don't run as root. Run as normal user without sudo.");
            }
            fs::create_dir_all(&config_dir).context("Failed to create directory during example config file creation")?;
            fs::write(&path, Self::example_config()).context("Failed to create example config file")?;
            bail!("Config file created at {:?}\nPlease edit it and run again.", path);
        }

        let contents = fs::read_to_string(&path)
            .context("Failed to read configuration file")?;
        let config: AppConfig = serde_yaml::from_str(&contents)
            .context("Error processing configuration file. Check formatting.")?;
        Ok(config)
    }

    fn example_config() -> &'static str {
        r###"
# Sample configuration file for two lights and single zone covering full 1080p monitor
# Enter mqtt options, define lights, and set zones that map to those lights in this file.
mqtt:
  name: "my-connection"
  broker: "192.168.1.100"
  port: 1883
  user: "user name"         # optional depending on broker config
  password: "password"      # optional depending on broker config

downsample_factor: 4

lights:
  - light_name: "your_device_name"    # Must match the device name in Z2M. Can be a Z2M group or single light
    service: "Zigbee2MQTT"
    brightness: 200

zone:
  - name: "main_screen"
    x: 0
    y: 0
    width: 1920
    height: 1080
    light_name: "your_device_name"  # Must match device_name of the lights imported above

performance:
  target_fps: 10
  min_fps: 1
  refresh_threshold: 10
"###
    }
}

///This struct and its methods are essentially a wrapper for the rumqttc client.
#[derive(Deserialize)]
pub struct MQTTConfig {
    pub name: String,
    pub broker: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
}

impl MQTTConfig {
    // This is a wrapper around rumqttc that creates the client and connection from the MQTTConfig thats created during app configuration (AppConfig.load)
    pub fn create_client(&self) -> Result<(Client, Connection)> {
        let mut mqttoptions = MqttOptions::new(&self.name, &self.broker, 1883);
        mqttoptions.set_keep_alive(Duration::from_secs(5));

        if let (Some(user), Some(password)) = (&self.user, &self.password) {
            mqttoptions.set_credentials(user, password);
        }

        let (client, connection) = Client::new(mqttoptions, 2); //use tiny cap for adaptive frame rate
        Ok((client, connection))
    }
}
