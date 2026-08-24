//! Zigbee2MQTT as a [`LightSink`].
//!
//! Everything Zigbee-specific lives here: how a command is addressed, which
//! fields are worth resending, how delivery failures are learned, and how a
//! light's previous state is read back so a session can hand it over on the way
//! out.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use zync_core::domain::{Config, LightCommand, LightId, LightService, LightSpec, StopPolicy};
use zync_core::ports::LightSink;

use crate::mqtt::{Message, MqttBus};

/// Zigbee2MQTT republishes its own log stream here. It is the only place the mesh
/// tells us a command was refused, since publishes are fire-and-forget.
const LOG_TOPIC: &str = "zigbee2mqtt/bridge/logging";

/// Below these deltas a field is left out of the payload. Zigbee2MQTT turns each
/// field into its own ZCL command, so resending an unchanged brightness costs a
/// whole extra network transaction for nothing visible.
const COLOR_RESEND_DELTA: u8 = 2;
const BRIGHTNESS_RESEND_DELTA: u8 = 4;

/// How long the lights get to report their current state at startup. Generous
/// enough for a mesh under load, short enough not to feel like a hang.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_millis(2500);

/// Fade applied when handing the lights back, so a session ends as a soft
/// transition rather than a snap.
const RESTORE_TRANSITION: f32 = 0.5;

#[derive(Deserialize)]
struct LogMessage {
    level: String,
    message: String,
}

/// Detects the "failed to send / BUSY" class of log entry that indicates the
/// Zigbee mesh is congested and we should back off.
fn parse_delivery_failure(payload: &[u8]) -> Option<LogMessage> {
    serde_json::from_slice::<LogMessage>(payload)
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

/// Per-light command state.
struct Light {
    spec: LightSpec,
    /// What the light was last told, after brightness scaling. Tracked per field
    /// so a skipped field is still compared against the value the light holds,
    /// and small drifts accumulate into a send rather than being lost forever.
    last_sent: Option<LightCommand>,
    /// State read back before the session started, if the light answered.
    previous: Option<Value>,
}

impl Light {
    fn set_topic(&self) -> String {
        format!("zigbee2mqtt/{}/set", self.spec.light_name)
    }

    fn get_topic(&self) -> String {
        format!("zigbee2mqtt/{}/get", self.spec.light_name)
    }

    fn state_topic(&self) -> String {
        format!("zigbee2mqtt/{}", self.spec.light_name)
    }

    /// Which fields differ enough from the last command to be worth a transaction.
    fn changed_fields(&self, next: LightCommand) -> (bool, bool) {
        match self.last_sent {
            None => (true, true),
            Some(previous) => {
                let color = [
                    (previous.color.r, next.color.r),
                    (previous.color.g, next.color.g),
                    (previous.color.b, next.color.b),
                ]
                .iter()
                .any(|(before, after)| before.abs_diff(*after) >= COLOR_RESEND_DELTA);
                let brightness =
                    previous.brightness.abs_diff(next.brightness) >= BRIGHTNESS_RESEND_DELTA;

                (color, brightness)
            }
        }
    }

    fn record(&mut self, next: LightCommand, color: bool, brightness: bool) {
        let mut recorded = self.last_sent.unwrap_or(next);
        if color {
            recorded.color = next.color;
        }
        if brightness {
            recorded.brightness = next.brightness;
        }
        self.last_sent = Some(recorded);
    }
}

pub struct Z2mSink {
    bus: Arc<MqttBus>,
    lights: HashMap<LightId, Light>,
    /// Configuration order, so startup and shutdown logging is stable.
    order: Vec<LightId>,
    policy: StopPolicy,
    failures: HashMap<LightId, Arc<AtomicU64>>,
}

impl Z2mSink {
    /// Builds a sink for every configured light and starts watching Zigbee2MQTT's
    /// log stream for delivery failures.
    pub fn new(bus: Arc<MqttBus>, config: &Config) -> Result<Self> {
        if let Some(other) = config
            .lights
            .iter()
            .find(|light| light.service != LightService::Zigbee2MQTT)
        {
            bail!(
                "Light '{}' uses {:?}, which is not implemented yet. Only Zigbee2MQTT is supported.",
                other.light_name,
                other.service
            );
        }

        let lights = config
            .lights
            .iter()
            .map(|spec| {
                let light = Light {
                    spec: spec.clone(),
                    last_sent: None,
                    previous: None,
                };
                (spec.light_name.clone(), light)
            })
            .collect();

        let failures = config
            .lights
            .iter()
            .map(|spec| (spec.light_name.clone(), Arc::new(AtomicU64::new(0))))
            .collect::<HashMap<_, _>>();

        let sink = Z2mSink {
            bus,
            lights,
            order: config.lights.iter().map(|l| l.light_name.clone()).collect(),
            policy: config.on_stop,
            failures,
        };
        sink.watch_failures()?;

        Ok(sink)
    }

    /// Charges reported failures to the light that caused them.
    fn watch_failures(&self) -> Result<()> {
        let logs = self.bus.subscribe(LOG_TOPIC)?;
        let counters = self.failures.clone();

        thread::Builder::new()
            .name("z2m-log".into())
            .spawn(move || {
                for message in logs {
                    let Some(log) = parse_delivery_failure(&message.payload) else {
                        continue;
                    };

                    match failed_light_name(&log.message)
                        .map(LightId::new)
                        .and_then(|name| counters.get(&name))
                    {
                        Some(counter) => {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                        // Unattributable failures still mean the mesh is
                        // struggling, so every light backs off rather than none.
                        None => {
                            for counter in counters.values() {
                                counter.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            })
            .context("Failed to start the Zigbee2MQTT log watcher")?;

        Ok(())
    }

    /// Applies the light's configured brightness ceiling.
    fn scale(&self, light: &Light, command: LightCommand) -> LightCommand {
        command.scaled(light.spec.brightness)
    }

    /// The payload that puts one light back where the stop policy says it belongs,
    /// or `None` when it should be left alone.
    fn restore_payload(&self, light: &Light) -> Option<Map<String, Value>> {
        let fallback = || light.spec.fallback_state.as_ref().and_then(as_object);

        let mut payload = match self.policy {
            StopPolicy::Hold => return None,
            StopPolicy::Off => {
                let mut off = Map::new();
                off.insert("state".into(), json!("OFF"));
                off
            }
            StopPolicy::Default => fallback()?,
            StopPolicy::Restore => light
                .previous
                .as_ref()
                .and_then(previous_state_payload)
                .or_else(fallback)?,
        };

        payload.insert("transition".into(), json!(RESTORE_TRANSITION));
        Some(payload)
    }
}

impl LightSink for Z2mSink {
    fn would_send(&self, light: &LightId, command: LightCommand) -> bool {
        let Some(light) = self.lights.get(light) else {
            return false;
        };
        let (color, brightness) = light.changed_fields(self.scale(light, command));

        color || brightness
    }

    /// Sends only the fields that changed.
    fn send(&mut self, light: &LightId, command: LightCommand) -> Result<bool> {
        let Some(state) = self.lights.get(light) else {
            bail!("No such light: {light}");
        };

        let next = self.scale(state, command);
        let (send_color, send_brightness) = state.changed_fields(next);
        if !send_color && !send_brightness {
            return Ok(false);
        }

        let mut payload = Map::new();
        if send_color {
            payload.insert(
                "color".into(),
                json!({ "r": next.color.r, "g": next.color.g, "b": next.color.b }),
            );
        }
        if send_brightness {
            payload.insert("brightness".into(), json!(next.brightness));
        }
        payload.insert("transition".into(), json!(next.transition));

        let topic = state.set_topic();
        self.bus
            .publish(&topic, Value::Object(payload).to_string().into_bytes())?;

        // Recorded only after the publish is accepted, so a rejected command does
        // not suppress the next attempt at the same colour.
        if let Some(state) = self.lights.get_mut(light) {
            state.record(next, send_color, send_brightness);
        }

        Ok(true)
    }

    fn failures(&self, light: &LightId) -> u64 {
        self.failures
            .get(light)
            .map_or(0, |counter| counter.load(Ordering::Relaxed))
    }

    /// Reads each light's current state so [`Self::restore`] can hand it back.
    ///
    /// The subscription is opened only for the duration of this call: the device
    /// state topic also carries the echo of our own commands, and an unread
    /// channel on it would grow for the whole session.
    fn snapshot(&mut self) -> Result<()> {
        if self.policy == StopPolicy::Hold || self.policy == StopPolicy::Off {
            return Ok(());
        }

        let filter = "zigbee2mqtt/+";
        let states = self.bus.subscribe(filter)?;

        for id in &self.order {
            if let Some(light) = self.lights.get(id) {
                // Zigbee2MQTT answers a read of any attribute by publishing the
                // device's whole state object, so asking for `state` is enough.
                self.bus
                    .publish(&light.get_topic(), br#"{"state":""}"#.to_vec())?;
            }
        }

        let collected = self.collect_states(states);
        if let Err(e) = self.bus.unsubscribe(filter) {
            debug!(error = ?e, "could not unsubscribe from the device state topic");
        }

        let missing: Vec<&LightId> = self
            .order
            .iter()
            .filter(|id| self.lights.get(*id).is_none_or(|l| l.previous.is_none()))
            .collect();

        if missing.is_empty() {
            debug!(lights = collected, "recorded previous light state");
        } else {
            // Expected for groups, which have no single state to read back.
            info!(
                recorded = collected,
                ?missing,
                "some lights did not report their state; they will fall back to their configured state on stop"
            );
        }

        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        if self.policy == StopPolicy::Hold {
            debug!("stop policy is hold; leaving the lights as they are");
            return Ok(());
        }

        let payloads: Vec<(String, Vec<u8>)> = self
            .order
            .iter()
            .filter_map(|id| self.lights.get(id))
            .filter_map(|light| {
                let payload = self.restore_payload(light)?;
                Some((
                    light.set_topic(),
                    Value::Object(payload).to_string().into_bytes(),
                ))
            })
            .collect();

        if payloads.is_empty() {
            warn!("no light had a state to return to; leaving them as they are");
            return Ok(());
        }

        // Reliable rather than fire-and-forget: this is the last thing the process
        // does, so a dropped publish would leave the lights showing a game frame.
        for (topic, payload) in &payloads {
            self.bus.publish_reliable(topic, payload.clone())?;
        }

        info!(lights = payloads.len(), policy = ?self.policy, "returned the lights to their previous state");
        Ok(())
    }
}

impl Z2mSink {
    /// Collects state reports until every light has answered or time runs out.
    fn collect_states(&mut self, states: std::sync::mpsc::Receiver<Message>) -> usize {
        let by_topic: HashMap<String, LightId> = self
            .lights
            .values()
            .map(|light| (light.state_topic(), light.spec.light_name.clone()))
            .collect();

        let deadline = Instant::now() + SNAPSHOT_TIMEOUT;
        let mut collected = 0;

        while collected < self.lights.len() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let Ok(message) = states.recv_timeout(remaining) else {
                break;
            };

            let Some(id) = by_topic.get(&message.topic) else {
                continue;
            };
            let Ok(state) = serde_json::from_slice::<Value>(&message.payload) else {
                continue;
            };

            if let Some(light) = self.lights.get_mut(id) {
                if light.previous.is_none() {
                    collected += 1;
                }
                light.previous = Some(state);
            }
        }

        collected
    }
}

fn as_object(value: &Value) -> Option<Map<String, Value>> {
    value.as_object().cloned()
}

/// Extracts the fields worth restoring from a Zigbee2MQTT state object.
///
/// Only one colour representation is sent: Zigbee2MQTT treats `color` and
/// `color_temp` as competing commands, and sending both leaves the light on
/// whichever happened to be applied last.
fn previous_state_payload(state: &Value) -> Option<Map<String, Value>> {
    let state = state.as_object()?;
    let mut payload = Map::new();

    for field in ["state", "brightness"] {
        if let Some(value) = state.get(field) {
            payload.insert(field.to_string(), value.clone());
        }
    }

    let in_color_temp_mode = state
        .get("color_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode == "color_temp");

    match (in_color_temp_mode, state.get("color_temp"), state.get("color")) {
        (true, Some(temp), _) => {
            payload.insert("color_temp".into(), temp.clone());
        }
        (false, _, Some(color)) => {
            payload.insert("color".into(), color.clone());
        }
        // No colour reported at all: a dimmable-only bulb, or a partial report.
        (_, temp, color) => {
            if let Some(value) = temp.or(color) {
                let key = if temp.is_some() { "color_temp" } else { "color" };
                payload.insert(key.to_string(), value.clone());
            }
        }
    }

    // A payload of nothing but a transition would turn into a no-op command.
    (!payload.is_empty()).then_some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zync_core::domain::Rgb;

    fn light(brightness: f32) -> Light {
        Light {
            spec: LightSpec {
                service: LightService::Zigbee2MQTT,
                light_name: LightId::new("lamp"),
                brightness,
                is_group: false,
                max_updates_per_sec: None,
                fallback_state: None,
            },
            last_sent: None,
            previous: None,
        }
    }

    fn command(r: u8, g: u8, b: u8, brightness: u8) -> LightCommand {
        LightCommand { color: Rgb::new(r, g, b), brightness, transition: 0.0 }
    }

    #[test]
    fn extracts_light_name_from_a_failure_line() {
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

    #[test]
    fn non_failure_log_lines_are_ignored() {
        let payload = br#"{"level":"error","message":"MQTT publish: topic 'x', payload 'y'","namespace":"z2m"}"#;

        assert!(parse_delivery_failure(payload).is_none());
    }

    #[test]
    fn the_first_command_sends_every_field() {
        assert_eq!(light(1.0).changed_fields(command(10, 20, 30, 100)), (true, true));
    }

    #[test]
    fn an_identical_command_sends_nothing() {
        let mut lamp = light(1.0);
        let sent = command(10, 20, 30, 100);
        lamp.last_sent = Some(sent);

        assert_eq!(lamp.changed_fields(sent), (false, false));
    }

    /// Each field costs its own ZCL transaction, so a colour change must not drag
    /// an unchanged brightness along with it.
    #[test]
    fn only_the_field_that_moved_is_resent() {
        let mut lamp = light(1.0);
        lamp.last_sent = Some(command(10, 20, 30, 100));

        assert_eq!(lamp.changed_fields(command(90, 20, 30, 100)), (true, false));
        assert_eq!(lamp.changed_fields(command(10, 20, 30, 200)), (false, true));
    }

    #[test]
    fn sub_threshold_drift_is_not_worth_a_transaction() {
        let mut lamp = light(1.0);
        lamp.last_sent = Some(command(10, 20, 30, 100));

        assert_eq!(lamp.changed_fields(command(11, 21, 31, 102)), (false, false));
    }

    /// Skipped fields still compare against what the light holds, so repeated
    /// small drifts eventually add up to a send instead of being lost.
    #[test]
    fn repeated_small_drifts_accumulate_into_a_send() {
        let mut lamp = light(1.0);
        lamp.last_sent = Some(command(10, 20, 30, 100));

        assert_eq!(lamp.changed_fields(command(11, 20, 30, 100)), (false, false));
        assert_eq!(lamp.changed_fields(command(12, 20, 30, 100)), (true, false));
    }

    #[test]
    fn a_colour_state_is_restored_as_colour() {
        let state = serde_json::json!({
            "state": "ON",
            "brightness": 180,
            "color_mode": "xy",
            "color": { "x": 0.4, "y": 0.35 },
            "linkquality": 60
        });

        let payload = previous_state_payload(&state).unwrap();

        assert_eq!(payload.get("state").unwrap(), "ON");
        assert_eq!(payload.get("brightness").unwrap(), 180);
        assert!(payload.contains_key("color"));
        assert!(!payload.contains_key("color_temp"), "colour modes must not compete");
        assert!(!payload.contains_key("linkquality"), "read-only fields must not be sent back");
    }

    #[test]
    fn a_white_state_is_restored_as_colour_temperature() {
        let state = serde_json::json!({
            "state": "ON",
            "brightness": 200,
            "color_mode": "color_temp",
            "color_temp": 370,
            "color": { "x": 0.45, "y": 0.4 }
        });

        let payload = previous_state_payload(&state).unwrap();

        assert_eq!(payload.get("color_temp").unwrap(), 370);
        assert!(!payload.contains_key("color"), "colour modes must not compete");
    }

    #[test]
    fn a_dimmable_only_bulb_restores_without_colour() {
        let state = serde_json::json!({ "state": "ON", "brightness": 120 });

        let payload = previous_state_payload(&state).unwrap();

        assert_eq!(payload.len(), 2);
        assert_eq!(payload.get("brightness").unwrap(), 120);
    }

    /// A payload carrying nothing but a transition is a command that does nothing,
    /// so an empty report must fall through to the configured fallback instead.
    #[test]
    fn a_state_report_with_nothing_usable_yields_no_payload() {
        assert!(previous_state_payload(&serde_json::json!({ "linkquality": 42 })).is_none());
        assert!(previous_state_payload(&serde_json::json!("not an object")).is_none());
    }

    #[test]
    fn topics_are_addressed_per_light() {
        let lamp = light(1.0);

        assert_eq!(lamp.set_topic(), "zigbee2mqtt/lamp/set");
        assert_eq!(lamp.get_topic(), "zigbee2mqtt/lamp/get");
        assert_eq!(lamp.state_topic(), "zigbee2mqtt/lamp");
    }
}
