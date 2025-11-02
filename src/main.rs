use std::thread;
use anyhow::Result;
use rumqttc::Client;
use std::collections::HashMap;

use crate::capture::{ZoneConfig, ZoneSampler, ScreenCapture};
use crate::config::AppConfig;
use crate::lights::*;
use crate::sync::{AdaptiveRate, SyncEngine, ZonePair};
mod config;
mod lights;
mod capture;
mod sync;


fn main() -> Result<()> {

    let config = AppConfig::load()?;
    let (client, mut connection) = config.mqtt.create_client()?;
    let adaptive_rate = AdaptiveRate::new_from_fps(
                            config.performance.target_fps,
                            config.performance.max_delay
    );
    let zone_map = extract_zones_and_lights(config.lights, config.zones, &client)?;
    let screen = ScreenCapture::new()?;
    let mut engine = SyncEngine::new(screen, zone_map, adaptive_rate, config.performance, config.downsample_factor);

    // start notification thread
    thread::spawn(move || {
        for notification in connection.iter().enumerate() {
            // println!("Notification = {:?}", notification);
        }
    });

    engine.run()?;
    Ok(())
}

fn extract_zones_and_lights(
    lights: Vec<LightConfig>,
    zones: Vec<ZoneConfig>,
    client: &Client,
) -> Result<Vec<ZonePair<'_>>>{

    //initialize LightController instances and assemble in light_controllers hashmap
    let mut light_controllers = HashMap::new();

    for light_config in lights {
        let light_controller = LightController::new(light_config, client);
        light_controllers.insert(light_controller.get_light_name(), light_controller);
    }

    //initialize ZoneSample instances, and assemble into zone_samplers vector
    let mut zone_samplers: Vec<ZoneSampler> = Vec::new();

    for zone in zones {
        let zone_sampler = ZoneSampler::new(zone)?;
        zone_samplers.push(zone_sampler);
    }

    //iterate through zone_samplers, look up associated light_controller, and push into zone_map<ZonePair> vector
    let mut zone_map: Vec<ZonePair> = Vec::new();

    for zone in zone_samplers {
        let light_controller = light_controllers.remove(&zone.get_light_name())
            .ok_or_else(|| anyhow::anyhow!("Zone references unknown light: {}", &zone.get_light_name()))?;
        let pair = ZonePair::new(zone, light_controller, None);
        zone_map.push(pair);
    }
    Ok(zone_map)
}
