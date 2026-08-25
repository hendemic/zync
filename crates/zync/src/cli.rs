//! Command line surface.
//!
//! Each subcommand is a thin translation from arguments into a call on the
//! adapters and the application layer. `regions` and `config` are added here
//! without the layers below needing to know.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use zync_adapters::lights::Z2mSink;
use zync_adapters::mqtt::{self, MqttBus};
use zync_adapters::config;
use zync_core::app::{ControlCommand, Supervisor, SyncLoop};
use zync_core::domain::Config;

use crate::service;

/// Time allowed for queued publishes — the restore commands especially — to
/// reach the broker before the process exits.
const FLUSH_GRACE: Duration = Duration::from_millis(400);

/// How long `stop` waits to see the service actually go away.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Start syncing the lights, in the background.
    Start {
        /// Run in this terminal instead of detaching, logging as it goes.
        #[arg(long, short)]
        foreground: bool,
    },
    /// Stop syncing and hand the lights back.
    Stop,
    /// Report whether zync is running.
    Status,
    /// Show what zync has been doing.
    Logs {
        /// Keep printing new output until interrupted.
        #[arg(long, short)]
        follow: bool,
        /// Show only the most recent run, in full.
        #[arg(long, short)]
        session: bool,
        /// Include the per-frame detail the log already holds.
        #[arg(long, short, conflicts_with = "level")]
        verbose: bool,
        /// Lowest level to show. Defaults to events only.
        #[arg(long, value_enum, default_value_t = service::LogLevel::Info)]
        level: service::LogLevel,
        /// How many existing lines to show first. Ignored with --session.
        #[arg(long, short = 'n', default_value_t = 40)]
        lines: usize,
    },
    /// The detached service process. Not for direct use.
    #[command(name = "__daemon", hide = true)]
    Daemon,
}

/// Which log destinations a command wants.
///
/// The parent of a detached service must not write to the same file as the
/// service itself, so only one process at a time logs to disk.
pub enum Logging {
    /// The service: the log file only, since it has no terminal.
    Service,
    /// A foreground run: the log file and the terminal.
    Foreground,
    /// A short-lived command: the terminal only.
    Client,
    /// Output belongs to the command itself; logs would only get in the way.
    Silent,
}

impl Command {
    pub fn logging(&self) -> Logging {
        match self {
            Command::Daemon => Logging::Service,
            Command::Start { foreground: true } => Logging::Foreground,
            Command::Logs { .. } => Logging::Silent,
            _ => Logging::Client,
        }
    }

    pub fn run(self) -> Result<()> {
        match self {
            Command::Start { foreground } => start(foreground),
            Command::Daemon => run_session(config::load_or_init()?),
            Command::Stop => stop(),
            Command::Status => status(),
            Command::Logs { follow, session, verbose, level, lines } => {
                service::tail(service::LogView {
                    level: if verbose { service::LogLevel::Debug } else { level },
                    session,
                    lines,
                    follow,
                })
            }
        }
    }
}

fn start(foreground: bool) -> Result<()> {
    // Loading here rather than in the child means a broken config is reported to
    // the person who typed the command, instead of only to a log file.
    let config = config::load_or_init()?;

    if foreground {
        return run_session(config);
    }

    if let Some(pid) = service::running_pid() {
        bail!("zync is already running (pid {pid}). Stop it first with `zync stop`.");
    }

    let pid = service::spawn_detached()?;
    let instance = config::resolve_instance(config.instance.as_deref());

    println!("zync is running in the background (pid {pid}, instance {instance}).");
    println!("  zync logs -f    follow what it is doing");
    println!("  zync logs -sv   everything from this run, in detail");
    println!("  zync stop       stop it and fade the lights back");

    Ok(())
}

/// Wires the adapters onto the application layer and runs until told to stop.
fn run_session(config: Config) -> Result<()> {
    // Held for the whole session, and removed on the way out, so `status` and
    // `stop` can find this process.
    let _pid_file = service::PidFile::acquire()?;

    let instance = config::resolve_instance(config.instance.as_deref());
    info!(instance, "{}", service::SESSION_MARKER);

    let bus = Arc::new(
        MqttBus::connect(&config.mqtt, &instance).context("Could not connect to MQTT")?,
    );
    let sink = Z2mSink::new(Arc::clone(&bus), &config)?;
    let frames = zync_capture::open()?;

    let session = SyncLoop::new(&config, frames, Box::new(sink))?;
    let (mut supervisor, control) = Supervisor::new(session);

    // Every way of stopping converges on the same channel, which is why none of
    // them needs its own teardown path — and why the lights are handed back the
    // same way whether the request came from a signal or from the network.
    mqtt::spawn_control_listener(&bus, &instance, control.clone())?;
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

    // Resolved the same way `start` resolves it, so a stop run on this machine
    // reaches this machine's instance and not another one on the same broker.
    let instance = config::resolve_instance(config.instance.as_deref());

    mqtt::request_shutdown(&config.mqtt, &instance)?;

    if service::await_exit(STOP_TIMEOUT) {
        println!("Stopped zync ({instance}). The lights are fading back.");
    } else {
        println!(
            "Asked zync ({instance}) to stop, but it is still running. See `zync logs`."
        );
    }

    Ok(())
}

fn status() -> Result<()> {
    let config_path = config::config_path()?;
    let configured = config::load_from(&config_path)
        .ok()
        .and_then(|config| config.instance);
    let instance = config::resolve_instance(configured.as_deref());

    match service::running_pid() {
        Some(pid) => println!("zync is running (pid {pid})."),
        None => println!("zync is not running."),
    }

    let log = service::current_log()
        .or_else(|| config::log_dir().ok())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "unavailable".to_string());

    println!("  instance  {instance}");
    println!("  config    {}", config_path.display());
    println!("  logs      {log}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap needs the name as a literal, so it cannot share the constant that
    /// `spawn_detached` passes. This is the guard against them drifting apart.
    #[test]
    fn the_hidden_daemon_command_matches_what_is_spawned() {
        assert!(
            Cli::command()
                .get_subcommands()
                .any(|sub| sub.get_name() == service::DAEMON_COMMAND),
            "no subcommand named {}",
            service::DAEMON_COMMAND
        );
    }

    #[test]
    fn start_detaches_unless_foreground_is_asked_for() {
        let background = Cli::try_parse_from(["zync", "start"]).unwrap();
        let foreground = Cli::try_parse_from(["zync", "start", "--foreground"]).unwrap();

        assert!(matches!(background.command, Command::Start { foreground: false }));
        assert!(matches!(foreground.command, Command::Start { foreground: true }));
    }

    /// The service writes the log file; a client process must not also open it.
    #[test]
    fn only_the_service_and_a_foreground_run_write_the_log_file() {
        let writes_file = |command: Command| {
            matches!(command.logging(), Logging::Service | Logging::Foreground)
        };

        assert!(writes_file(Command::Daemon));
        assert!(writes_file(Command::Start { foreground: true }));
        assert!(!writes_file(Command::Start { foreground: false }));
        assert!(!writes_file(Command::Stop));
        assert!(!writes_file(Command::Status));
        assert!(!writes_file(Command::Logs {
            follow: false,
            session: false,
            verbose: false,
            level: service::LogLevel::Info,
            lines: 40,
        }));
    }
}
