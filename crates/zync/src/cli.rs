//! Command line surface.
//!
//! Each subcommand is a thin translation from arguments into a call on the
//! adapters and the application layer. New commands — `config`, `regions`,
//! `doctor` — are added here without the layers below needing to know.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use zync_adapters::lights::Z2mSink;
use zync_adapters::mqtt::{self, MqttBus};
use zync_adapters::{config, open_frame_source};
use zync_core::app::{ControlCommand, Supervisor, SyncLoop};

/// Time allowed for queued publishes — the restore commands especially — to
/// reach the broker before the process exits.
const FLUSH_GRACE: Duration = Duration::from_millis(400);

#[derive(Parser)]
#[command(
    name = "zync",
    version,
    about = "Drive Zigbee lights from what is on your screen"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Sync the lights to the screen. Runs in the foreground until stopped.
    Start,
    /// Ask a running instance to stop and hand the lights back.
    Stop,
}

impl Command {
    pub fn run(self) -> Result<()> {
        match self {
            Command::Start => start(),
            Command::Stop => stop(),
        }
    }
}

/// Wires the adapters onto the application layer and runs until told to stop.
fn start() -> Result<()> {
    let config = config::load_or_init()?;

    let bus = Arc::new(MqttBus::connect(&config.mqtt).context("Could not connect to MQTT")?);
    let sink = Z2mSink::new(Arc::clone(&bus), &config)?;
    let frames = open_frame_source()?;

    let session = SyncLoop::new(&config, frames, Box::new(sink))?;
    let (mut supervisor, control) = Supervisor::new(session);

    // Every way of stopping converges on the same channel, which is why none of
    // them needs its own teardown path — and why the lights are handed back the
    // same way whether the request came from a signal or from the network.
    mqtt::spawn_control_listener(&bus, &config.mqtt.name, control.clone())?;
    let on_signal = control.clone();
    ctrlc::set_handler(move || {
        info!("interrupt received; stopping");
        let _ = on_signal.send(ControlCommand::Shutdown);
    })
    .context("Failed to install the interrupt handler")?;

    if let Err(e) = bus.announce_online() {
        warn!(error = ?e, "could not announce this instance; `zync stop` may not find it");
    }

    let outcome = supervisor.run();

    bus.announce_offline();
    bus.flush(FLUSH_GRACE);

    outcome.map(|_| ())
}

fn stop() -> Result<()> {
    let config = config::load_from(&config::config_path()?)?;

    mqtt::request_shutdown(&config.mqtt)?;
    println!("Stopping zync.");

    Ok(())
}
