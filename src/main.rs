use std::thread;
use anyhow::{Context, Result};
use rumqttc::{Client, Event, Packet, QoS};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::capture::{ZoneConfig, ZoneSampler, new_screen};
use crate::config::AppConfig;
use crate::lights::*;
use crate::sync::{AdaptiveRate, SyncEngine, ZonePair};
mod config;
mod lights;
mod capture;
mod sync;

/// Zigbee2MQTT republishes its own log stream here. It is the only place the
/// mesh tells us a command was refused, since publishes are fire-and-forget.
const Z2M_LOG_TOPIC: &str = "zigbee2mqtt/bridge/logging";

/// One failure counter per configured light, keyed by its Zigbee2MQTT name.
type FailureCounters = HashMap<String, Arc<AtomicU64>>;

#[derive(Deserialize)]
struct Z2MLogMessage {
    level: String,
    message: String,
}

/// Detects the "failed to send / BUSY" class of log entry that indicates the
/// Zigbee mesh is congested and we should back off.
fn parse_delivery_failure(payload: &[u8]) -> Option<Z2MLogMessage> {
    serde_json::from_slice::<Z2MLogMessage>(payload)
        .ok()
        .filter(|log| log.level == "error" && log.message.contains("failed"))
}

/// Pulls the light name out of Zigbee2MQTT's
/// `Publish 'set' 'color' to '<name>' failed: ...` wording, so a failure can be
/// charged to the light that caused it rather than to every light.
fn failed_light_name(message: &str) -> Option<&str> {
    let start = message.find(" to '")? + " to '".len();
    let end = message[start..].find('\'')? + start;
    Some(&message[start..end])
}

fn main() -> Result<()> {

    // Load configuratoin and initialize all objects to pass into sync engine
    let config = AppConfig::load()?;
    let (client, mut connection) = config.mqtt.create_client()?;

    // The capture source is created first because it defines the coordinate space
    // zones are written in. Users configure zones at native display resolution;
    // translating those onto whatever the capture pipeline delivers is our job.
    let screen = new_screen()?;
    let source_size = screen.source_size();

    // Worth printing plainly: if the size does not match the monitor you expected,
    // the portal handed us a single window rather than the whole display; and the
    // capture mode decides whether fullscreen apps can be captured on GNOME.
    println!(
        "Capture source: {}x{} (zones are configured in these coordinates)\nCapture mode: {}",
        source_size.0,
        source_size.1,
        screen.describe()
    );

    let failure_counters: FailureCounters = config
        .lights
        .iter()
        .map(|light| (light.light_name.clone(), Arc::new(AtomicU64::new(0))))
        .collect();

    let adaptive_rate = AdaptiveRate::new_from_fps(
                            config.performance.max_fps,
                            config.performance.max_delay,
                            config.performance.percent_thread_work,
    );
    let zone_map = extract_zones_and_lights(config.lights, config.zones, &client, source_size, &failure_counters)?;

    client.subscribe(Z2M_LOG_TOPIC, QoS::AtMostOnce)
        .context("Failed to subscribe to the Zigbee2MQTT log topic")?;

    // create SyncEngine -- this is the main loop that runs the program
    let mut engine = SyncEngine::new(screen, zone_map, adaptive_rate, config.performance, config.downsample_factor);

    // start notification thread. Draining this also drives the MQTT event loop, so
    // it is required regardless of whether we inspect the messages.
    thread::spawn(move || {
        for event in connection.iter() {
            let Ok(Event::Incoming(Packet::Publish(publish))) = event else {
                continue;
            };
            if publish.topic != Z2M_LOG_TOPIC {
                continue;
            }
            let Some(log) = parse_delivery_failure(&publish.payload) else {
                continue;
            };

            match failed_light_name(&log.message).and_then(|name| failure_counters.get(name)) {
                Some(counter) => {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                // Unattributable failures still mean the mesh is struggling, so
                // every light backs off rather than none.
                None => {
                    for counter in failure_counters.values() {
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    });

    // start main thread
    engine.run()?;
    Ok(())
}

fn extract_zones_and_lights<'a>(
    lights: Vec<LightConfig>,
    zones: Vec<ZoneConfig>,
    client: &'a Client,
    source_size: (u32, u32),
    failure_counters: &FailureCounters,
) -> Result<Vec<ZonePair<'a>>>{

    //initialize LightController instances and assemble in light_controllers hashmap
    let mut light_controllers = HashMap::new();

    for light_config in lights {
        let light_controller = LightController::new(light_config, client);
        light_controllers.insert(light_controller.get_light_name(), light_controller);
    }

    //initialize ZoneSample instances, and assemble into zone_samplers vector
    let mut zone_samplers: Vec<ZoneSampler> = Vec::new();

    for zone in zones {
        let zone_sampler = ZoneSampler::new(zone, source_size)?;
        zone_samplers.push(zone_sampler);
    }

    //iterate through zone_samplers, look up associated light_controller, and push into zone_map<ZonePair> vector
    let mut zone_map: Vec<ZonePair> = Vec::new();

    for zone in zone_samplers {
        let name = zone.get_light_name();
        let light_controller = light_controllers.remove(&name)
            .ok_or_else(|| anyhow::anyhow!("Zone references unknown light: {}", name))?;
        let failures = failure_counters
            .get(&name)
            .map(Arc::clone)
            .ok_or_else(|| anyhow::anyhow!("No failure counter for light: {}", name))?;
        zone_map.push(ZonePair::new(zone, light_controller, failures));
    }
    Ok(zone_map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_light_name_from_z2m_failure_line() {
        let message = "Publish 'set' 'color' to 'mikes-office-monitor-top' failed: 'Error: Command 12 lightingColorCtrl.moveToColor(...) failed (~x~> [ZCL GROUP groupId=12] Failed to send with status=BUSY.)'";

        assert_eq!(failed_light_name(message), Some("mikes-office-monitor-top"));
    }

    #[test]
    fn unrelated_failure_lines_have_no_light_name() {
        assert_eq!(failed_light_name("Delivery of MULTICAST failed for '65533'."), None);
    }

    #[test]
    fn only_error_level_failures_count() {
        let error = br#"{"level":"error","message":"Publish 'set' 'color' to 'x' failed: BUSY","namespace":"z2m"}"#;
        let info = br#"{"level":"info","message":"something failed but only informationally","namespace":"z2m"}"#;

        assert!(parse_delivery_failure(error).is_some());
        assert!(parse_delivery_failure(info).is_none());
    }
}
