//! What the commands do, with nothing said about how it looks.
//!
//! Every inbound adapter — the CLI next door, and whatever grows beside it —
//! calls these and decides for itself how to present the result. Nothing here
//! writes to stdout or stderr, and nothing here reads the terminal: a function
//! returns data or a typed outcome, including for the cases a UI wants to show
//! rather than treat as a failure. `tracing` is the one exception, and where
//! that goes is `main`'s decision.

use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use zync_adapters::config;
use zync_adapters::lights::Z2mSink;
use zync_adapters::mqtt::{self, MqttBus};
use zync_core::app::{ControlCommand, Supervisor, SyncLoop};
use zync_core::domain::Config;

use crate::service;

pub use crate::service::{FOLLOW_INTERVAL, LogFollower, LogLevel, LogView, SESSION_MARKER};

/// Time allowed for queued publishes — the restore commands especially — to
/// reach the broker before the process exits.
const FLUSH_GRACE: Duration = Duration::from_millis(400);

/// How long `stop` waits to see the service actually go away.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// What is known about this installation right now.
pub struct Status {
    /// The running service, or `None` when nothing is running.
    pub pid: Option<u32>,
    pub instance: String,
    pub config_path: PathBuf,
    /// The log file currently being written, falling back to the directory it
    /// would appear in. `None` when neither can be resolved.
    pub log_path: Option<PathBuf>,
    /// The config as it stands on disk. Kept as a result because a broken config
    /// must not stop status from answering the question it was asked; the
    /// instance above is still resolved, from the hostname if need be.
    ///
    /// The CLI reads nothing out of it — its `status` output is fixed — but a
    /// front end that can show the config itself, or the reason it is unusable,
    /// should not have to load it a second time to do so.
    #[allow(dead_code, reason = "part of the operation's answer, not every caller's question")]
    pub config: Result<Config>,
}

/// Reports on the service and the files around it.
///
/// Fails only when this user has no resolvable config directory, since then
/// every path in the answer would be invented.
pub fn status() -> Result<Status> {
    let config_path = config::config_path()?;
    let config = config::load_from(&config_path);
    let configured = config
        .as_ref()
        .ok()
        .and_then(|config| config.instance.clone());

    Ok(Status {
        pid: service::running_pid(),
        instance: config::resolve_instance(configured.as_deref()),
        config_path,
        log_path: service::current_log().or_else(|| config::log_dir().ok()),
        config,
    })
}

/// A service that is now running in the background.
pub struct Started {
    pub pid: u32,
    pub instance: String,
}

/// Launches the sync as a detached service.
///
/// The config is loaded here rather than in the child so that a problem with it
/// is reported to whoever asked for the start, instead of only to a log file.
/// That includes first run, where there is no config yet: writing the example is
/// a hard stop, because a generated config points at a broker that does not
/// exist and carrying on with it would look like a connection bug.
pub fn start() -> Result<Started> {
    let config = config::load_or_init()?;

    if let Some(pid) = service::running_pid() {
        bail!("zync is already running (pid {pid}). Stop it first with `zync stop`.");
    }

    let pid = service::spawn_detached()?;

    Ok(Started {
        pid,
        instance: config::resolve_instance(config.instance.as_deref()),
    })
}

/// Runs the sync in this process until it is told to stop.
///
/// The detached service takes this same path. The only difference between the
/// two is where the logs go, and that is settled by `Command::logging` before
/// anything here runs.
pub fn run_foreground() -> Result<()> {
    run_session(config::load_or_init()?)
}

/// Wires the adapters onto the application layer and runs until told to stop.
pub fn run_session(config: Config) -> Result<()> {
    // Held for the whole session, and removed on the way out, so `status` and
    // `stop` can find this process.
    let _pid_file = service::PidFile::acquire()?;

    let instance = config::resolve_instance(config.instance.as_deref());
    info!(instance, "{}", SESSION_MARKER);

    let bus =
        Arc::new(MqttBus::connect(&config.mqtt, &instance).context("Could not connect to MQTT")?);
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

/// The outcome of asking the service to stop.
pub struct Stopped {
    pub instance: String,
    /// Whether the service was seen to go away within the time allowed. False
    /// means the request was sent and the process is still there.
    pub exited: bool,
}

/// Asks the running instance to stop, and waits up to `STOP_TIMEOUT` to see it
/// happen.
///
/// Waiting is the point: it is what lets the caller say what happened rather
/// than only that a request was sent. It also means this blocks for up to five
/// seconds, so a UI with an event loop to keep serving should call it on a
/// worker thread, or call `stop_within` with a budget of its own.
pub fn stop() -> Result<Stopped> {
    stop_within(STOP_TIMEOUT)
}

/// `stop`, with the wait for exit bounded by `timeout`.
pub fn stop_within(timeout: Duration) -> Result<Stopped> {
    let config = config::load_from(&config::config_path()?)?;

    // Resolved the same way `start` resolves it, so a stop run on this machine
    // reaches this machine's instance and not another one on the same broker.
    let instance = config::resolve_instance(config.instance.as_deref());

    mqtt::request_shutdown(&config.mqtt, &instance)?;

    Ok(Stopped {
        exited: service::await_exit(timeout),
        instance,
    })
}
