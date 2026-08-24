//! One broker connection, several logical users.
//!
//! Light commands, Zigbee2MQTT's log stream, device state read-back, and remote
//! control all travel over the same connection. [`MqttBus`] owns it and its event
//! thread, and hands out per-topic receivers so each of those can be written as
//! if it had the connection to itself.

use anyhow::{Context, Result, bail};
use rumqttc::{Client, Connection, Event, LastWill, MqttOptions, Packet, QoS};
use std::collections::HashSet;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use zync_core::app::ControlCommand;
use zync_core::domain::MqttConfig;

const KEEP_ALIVE: Duration = Duration::from_secs(5);
/// Small on purpose. A deep queue only lets stale light commands accumulate, and
/// the pacing upstream is what is supposed to prevent a backlog in the first place.
const REQUEST_CAPACITY: usize = 10;

/// How long `zync stop` waits to hear that a running instance exists, and then to
/// see its own request acknowledged.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

pub const ONLINE: &str = "online";
pub const OFFLINE: &str = "offline";
pub const SHUTDOWN: &str = "shutdown";

/// Retained, so a controller learns whether an instance is running without
/// waiting for it to say anything. Also the last will, so an instance that dies
/// without warning stops claiming to be online.
pub fn status_topic(instance: &str) -> String {
    format!("zync/{instance}/status")
}

pub fn control_topic(instance: &str) -> String {
    format!("zync/{instance}/control")
}

/// The broker-facing identity of a running instance.
///
/// Built from the instance name rather than `mqtt.name` alone because a client id
/// must be unique per broker: MQTT 3.1.1 §3.1.4 has the broker disconnect the
/// older client on a collision, and since the event loop reconnects, two machines
/// sharing an id would disconnect each other indefinitely. `mqtt.name` is kept as
/// the prefix so the connection is still recognisable in broker logs.
fn client_id(config: &MqttConfig, instance: &str) -> String {
    format!("{}-{}", config.name, instance)
}

/// A message delivered to one subscriber.
#[derive(Clone, Debug)]
pub struct Message {
    pub topic: String,
    pub payload: Vec<u8>,
}

struct Route {
    filter: String,
    tx: Sender<Message>,
}

pub struct MqttBus {
    client: Client,
    routes: Arc<Mutex<Vec<Route>>>,
    status_topic: String,
}

impl MqttBus {
    /// Connects and starts the event thread.
    ///
    /// Draining the event loop is not optional: it is what makes any progress at
    /// all happen, including outbound publishes.
    pub fn connect(config: &MqttConfig, instance: &str) -> Result<Self> {
        let status_topic = status_topic(instance);
        let id = client_id(config, instance);
        debug!(client_id = %id, instance, "connecting to the MQTT broker");

        let mut options = MqttOptions::new(&id, &config.broker, config.port);
        options.set_keep_alive(KEEP_ALIVE);
        options.set_last_will(LastWill::new(
            &status_topic,
            OFFLINE,
            QoS::AtLeastOnce,
            true,
        ));

        if let (Some(user), Some(password)) = (&config.user, &config.password) {
            options.set_credentials(user, password);
        }

        let (client, connection) = Client::new(options, REQUEST_CAPACITY);
        let routes = Arc::new(Mutex::new(Vec::new()));

        thread::Builder::new()
            .name("mqtt".into())
            .spawn({
                let routes = Arc::clone(&routes);
                let client = client.clone();
                let status = status_topic.clone();
                move || pump(connection, routes, client, status)
            })
            .context("Failed to start the MQTT event thread")?;

        Ok(MqttBus { client, routes, status_topic })
    }

    /// Registers interest in a topic filter and subscribes on the broker.
    ///
    /// Supports the usual `+` and `#` wildcards. Dropping the receiver
    /// unregisters the route on the next matching message.
    pub fn subscribe(&self, filter: &str) -> Result<Receiver<Message>> {
        let (tx, rx) = channel();

        self.routes
            .lock()
            .map_err(|_| anyhow::anyhow!("MQTT route table poisoned"))?
            .push(Route { filter: filter.to_string(), tx });

        self.client
            .subscribe(filter, QoS::AtMostOnce)
            .with_context(|| format!("Failed to subscribe to {filter}"))?;

        Ok(rx)
    }

    /// Stops delivering a filter and drops its routes.
    ///
    /// Matters for subscriptions that are only wanted briefly: an unread channel
    /// on a busy topic grows without bound for as long as it stays registered.
    pub fn unsubscribe(&self, filter: &str) -> Result<()> {
        if let Ok(mut routes) = self.routes.lock() {
            routes.retain(|route| route.filter != filter);
        }

        self.client
            .unsubscribe(filter)
            .with_context(|| format!("Failed to unsubscribe from {filter}"))
    }

    /// Fire-and-forget publish, for anything on the light-command path. Never
    /// blocks the sync loop waiting on the network.
    pub fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<()> {
        self.client
            .try_publish(topic, QoS::AtMostOnce, false, payload)
            .with_context(|| format!("Failed to publish to {topic}"))
    }

    /// Blocking publish for messages that must not be dropped, such as the
    /// restore commands sent while shutting down.
    pub fn publish_reliable(&self, topic: &str, payload: Vec<u8>) -> Result<()> {
        self.client
            .publish(topic, QoS::AtLeastOnce, false, payload)
            .with_context(|| format!("Failed to publish to {topic}"))
    }

    /// Announces this instance as running, retained so a controller can find it.
    pub fn announce_online(&self) -> Result<()> {
        self.client
            .publish(&self.status_topic, QoS::AtLeastOnce, true, ONLINE)
            .context("Failed to publish the online status")
    }

    /// Clears the retained online marker on a clean exit, so `zync stop` does not
    /// report a stale instance. The last will covers the unclean case.
    pub fn announce_offline(&self) {
        if let Err(e) = self
            .client
            .publish(&self.status_topic, QoS::AtLeastOnce, true, OFFLINE)
        {
            debug!(error = ?e, "could not clear the online status");
        }
    }

    /// Lets the event thread flush what is queued before the process exits.
    /// Publishes are asynchronous, so exiting immediately after `restore` would
    /// drop the very commands that put the lights back.
    pub fn flush(&self, grace: Duration) {
        thread::sleep(grace);
        if let Err(e) = self.client.try_disconnect() {
            debug!(error = ?e, "could not close the MQTT connection cleanly");
        }
    }
}

/// Turns control messages into supervisor commands.
///
/// The listener is why `zync stop` needs no mechanism of its own, and why the
/// Home Assistant switch will not either: both are just publishes to this topic.
pub fn spawn_control_listener(
    bus: &MqttBus,
    instance: &str,
    commands: Sender<ControlCommand>,
) -> Result<()> {
    let topic = control_topic(instance);
    let messages = bus.subscribe(&topic)?;

    thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            for message in messages {
                let body = String::from_utf8_lossy(&message.payload);
                match body.trim() {
                    SHUTDOWN => {
                        info!("shutdown requested over MQTT");
                        if commands.send(ControlCommand::Shutdown).is_err() {
                            return;
                        }
                    }
                    other => warn!(command = other, "ignoring unrecognised control message"),
                }
            }
        })
        .context("Failed to start the control listener")?;

    debug!(topic, "listening for control messages");
    Ok(())
}

/// Asks a running instance to stop, from a separate short-lived process.
///
/// Checks the retained status topic first so that "nothing is running" is
/// reported as such rather than as a publish that silently went nowhere.
pub fn request_shutdown(config: &MqttConfig, instance: &str) -> Result<()> {
    // A distinct client id matters: reusing the running instance's id would make
    // the broker disconnect it, stopping the sync without restoring the lights.
    let id = format!("{}-ctl", client_id(config, instance));
    let mut options = MqttOptions::new(id, &config.broker, config.port);
    options.set_keep_alive(KEEP_ALIVE);

    if let (Some(user), Some(password)) = (&config.user, &config.password) {
        options.set_credentials(user, password);
    }

    let (client, mut connection) = Client::new(options, REQUEST_CAPACITY);
    let status = status_topic(instance);
    client
        .subscribe(&status, QoS::AtMostOnce)
        .context("Failed to subscribe to the status topic")?;

    let deadline = Instant::now() + CONTROL_TIMEOUT;
    if !await_online(&mut connection, &status, deadline)? {
        // Naming the instance matters: the usual cause is a name mismatch between
        // this config and the one the running process started with.
        bail!(
            "No running instance named '{instance}' found on {}. Nothing to stop.",
            config.broker
        );
    }

    client
        .publish(control_topic(instance), QoS::AtLeastOnce, false, SHUTDOWN)
        .context("Failed to publish the shutdown request")?;

    await_ack(&mut connection, deadline)
}

/// Waits for the retained status message, treating anything other than `online`
/// as nothing running.
fn await_online(connection: &mut Connection, status: &str, deadline: Instant) -> Result<bool> {
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match connection.recv_timeout(remaining) {
            Ok(Ok(Event::Incoming(Packet::Publish(publish)))) if publish.topic == status => {
                return Ok(publish.payload.as_ref() == ONLINE.as_bytes());
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => bail!("Could not reach the MQTT broker: {e}"),
            Err(_) => break,
        }
    }

    // No retained marker at all: the broker may have been restarted since the
    // instance started, so this is not proof either way.
    Ok(false)
}

fn await_ack(connection: &mut Connection, deadline: Instant) -> Result<()> {
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match connection.recv_timeout(remaining) {
            Ok(Ok(Event::Incoming(Packet::PubAck(_)))) => return Ok(()),
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => bail!("Could not reach the MQTT broker: {e}"),
            Err(_) => break,
        }
    }

    bail!("The broker did not acknowledge the shutdown request")
}

/// Drives the connection and fans incoming publishes out to their subscribers.
fn pump(
    mut connection: Connection,
    routes: Arc<Mutex<Vec<Route>>>,
    client: Client,
    status_topic: String,
) {
    let mut reconnecting = false;

    for event in connection.iter() {
        match event {
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                let message = Message {
                    topic: publish.topic,
                    payload: publish.payload.to_vec(),
                };
                if let Ok(mut routes) = routes.lock() {
                    // A closed receiver means its owner is gone; drop the route
                    // rather than keep matching against it forever.
                    routes.retain(|route| {
                        !matches(&route.filter, &message.topic)
                            || route.tx.send(message.clone()).is_ok()
                    });
                }
            }
            Ok(Event::Incoming(Packet::ConnAck(ack))) => {
                info!(session_present = ack.session_present, "connected to the MQTT broker");
                // The first connection's subscriptions are already queued by
                // `subscribe`; only a reconnect has lost them.
                if reconnecting {
                    restore_session(&routes, &client, &status_topic);
                }
                reconnecting = true;
            }
            Ok(_) => {}
            // The event loop reconnects on its own, so this is worth reporting
            // but not worth tearing anything down over.
            Err(e) => debug!(error = %e, "MQTT connection error; retrying"),
        }
    }
}

/// Re-establishes everything a reconnect silently dropped.
///
/// rumqttc never resubscribes, and its default clean session means the broker
/// does not remember either. Without this, a broker restart or a dropped
/// connection stops delivery-failure feedback and stops `zync stop` from being
/// heard, while the app carries on looking perfectly healthy — and `zync stop`
/// would still see its own publish acknowledged by the broker.
///
/// Everything here is non-blocking on purpose: this runs on the thread that has
/// to drain the event loop, so a blocking send would deadlock against itself.
fn restore_session(routes: &Arc<Mutex<Vec<Route>>>, client: &Client, status_topic: &str) {
    let filters: Vec<String> = {
        let Ok(routes) = routes.lock() else {
            return;
        };
        let mut seen = HashSet::new();
        routes
            .iter()
            .filter(|route| seen.insert(route.filter.as_str()))
            .map(|route| route.filter.clone())
            .collect()
    };

    for filter in &filters {
        if let Err(e) = client.try_subscribe(filter.as_str(), QoS::AtMostOnce) {
            warn!(filter, error = %e, "could not restore a subscription after reconnecting");
        }
    }

    // The last will published `offline` when the connection dropped, so without
    // this `zync stop` would report that nothing is running.
    if let Err(e) = client.try_publish(status_topic, QoS::AtLeastOnce, true, ONLINE) {
        warn!(error = %e, "could not re-announce this instance after reconnecting");
    }

    info!(subscriptions = filters.len(), "restored session after reconnecting");
}

/// MQTT topic filter matching: `+` covers one level, `#` covers the rest.
fn matches(filter: &str, topic: &str) -> bool {
    let mut topic_levels = topic.split('/');

    for level in filter.split('/') {
        match level {
            "#" => return true,
            "+" => {
                if topic_levels.next().is_none() {
                    return false;
                }
            }
            expected => {
                if topic_levels.next() != Some(expected) {
                    return false;
                }
            }
        }
    }

    topic_levels.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_topics_match_only_themselves() {
        assert!(matches("zigbee2mqtt/lamp", "zigbee2mqtt/lamp"));
        assert!(!matches("zigbee2mqtt/lamp", "zigbee2mqtt/other"));
    }

    #[test]
    fn a_single_level_wildcard_does_not_span_levels() {
        assert!(matches("zigbee2mqtt/+", "zigbee2mqtt/lamp"));
        assert!(!matches("zigbee2mqtt/+", "zigbee2mqtt/lamp/set"));
        assert!(!matches("zigbee2mqtt/+", "zigbee2mqtt"));
    }

    #[test]
    fn a_multi_level_wildcard_spans_the_remainder() {
        assert!(matches("zigbee2mqtt/#", "zigbee2mqtt/lamp/set"));
        assert!(matches("#", "anything/at/all"));
    }

    /// The device state topic and its own `set` sibling must not be confused, or
    /// the sink would read back its own commands as if they were device state.
    #[test]
    fn a_state_topic_does_not_match_its_set_sibling() {
        assert!(!matches("zigbee2mqtt/lamp", "zigbee2mqtt/lamp/set"));
        assert!(!matches("zigbee2mqtt/lamp/set", "zigbee2mqtt/lamp"));
    }

    #[test]
    fn shorter_topics_do_not_match_longer_filters() {
        assert!(!matches("a/b/c", "a/b"));
    }

    #[test]
    fn control_and_status_topics_are_namespaced_per_instance() {
        assert_eq!(control_topic("desk"), "zync/desk/control");
        assert_eq!(status_topic("desk"), "zync/desk/status");
        assert_ne!(control_topic("desk"), control_topic("laptop"));
    }

    fn broker(name: &str) -> MqttConfig {
        MqttConfig {
            name: name.into(),
            broker: "localhost".into(),
            port: 1883,
            user: None,
            password: None,
        }
    }

    /// The collision that matters: one config copied to two machines must still
    /// produce two client ids, or the broker disconnects each in turn forever.
    #[test]
    fn one_config_on_two_machines_yields_distinct_client_ids() {
        let config = broker("my-connection");

        assert_ne!(
            client_id(&config, "desk"),
            client_id(&config, "laptop"),
            "the instance name must reach the client id"
        );
    }

    /// `zync stop` connects alongside the instance it is stopping, so its id has
    /// to differ from that instance's too.
    #[test]
    fn the_control_client_does_not_collide_with_the_instance() {
        let config = broker("my-connection");
        let instance = client_id(&config, "desk");

        assert_ne!(format!("{instance}-ctl"), instance);
    }

    #[test]
    fn the_configured_name_stays_visible_in_the_client_id() {
        assert!(client_id(&broker("my-connection"), "desk").starts_with("my-connection"));
    }
}
