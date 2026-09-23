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
use std::process::{Command, Stdio};
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

/// Where `edit_config` turns when neither VISUAL nor EDITOR says otherwise.
/// Not friendly, but present on every Linux and macOS install, which is the
/// only property a fallback needs.
const FALLBACK_EDITOR: &str = "vi";

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
    /// The CLI reads nothing out of it — its `status` output is fixed — but the
    /// TUI shows the reason a config is unusable in its header, and should not
    /// have to load the file a second time to do so.
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

/// What became of an edit: where the file is, whether what was saved is usable,
/// and whether anything has to be restarted for it to take effect.
pub struct EditOutcome {
    pub path: PathBuf,
    /// The config as saved. A result rather than an error off the front of
    /// `edit_config`, so a UI can show the problem and offer another go at the
    /// file instead of unwinding the whole operation.
    pub validation: Result<Config>,
    /// A running service read its config when it started and will not see this
    /// one.
    pub restart_needed: bool,
}

/// Opens the config in the user's editor, waits for it, and reports on what was
/// saved.
///
/// The commented example is written first if there is no file yet, so the editor
/// always has something to open — the same file `zync start` would have created,
/// which is why first run through here does not need a start first.
pub fn edit_config() -> Result<EditOutcome> {
    let path = config::ensure_config()?;
    let command = editor_command(
        readable_env("VISUAL").as_deref(),
        readable_env("EDITOR").as_deref(),
    );
    // `editor_command` always yields at least the fallback, so this only guards
    // against that stopping being true.
    let (program, args) = command.split_first().context("No editor to run")?;

    // The editor owns the terminal for as long as it runs, which is why this
    // operation's command logs nowhere.
    let status = Command::new(program)
        .args(args)
        .arg(&path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| {
            format!("Could not run `{program}`. Set VISUAL or EDITOR to an editor you have.")
        })?;

    // An editor that failed says nothing about the file, so neither do we: a
    // "config is valid" line here would be about whatever was there before.
    if !status.success() {
        bail!(
            "`{program}` exited with {status}; {} has not been checked.",
            path.display()
        );
    }

    Ok(EditOutcome {
        restart_needed: service::running_pid().is_some(),
        validation: config::load_from(&path),
        path,
    })
}

/// Loads and validates the config from the standard path.
pub fn check_config() -> Result<Config> {
    config::load_from(&config_path()?)
}

/// Where the config lives, whether or not it exists yet.
pub fn config_path() -> Result<PathBuf> {
    config::config_path()
}

/// A variable's value, with unset and not-valid-unicode treated alike: neither
/// can be turned into a command, so both read as absent and the next choice gets
/// its turn.
fn readable_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// VISUAL, then EDITOR, then `vi`.
///
/// A value may carry arguments — `code --wait`, `emacsclient -nw` — so it is
/// split on whitespace rather than taken as a bare program name. A value that is
/// set but blank is treated as unset, since it names no editor either.
fn editor_command(visual: Option<&str>, editor: Option<&str>) -> Vec<String> {
    visual
        .into_iter()
        .chain(editor)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(|value| value.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_else(|| vec![FALLBACK_EDITOR.to_owned()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_wins_over_editor() {
        assert_eq!(editor_command(Some("nvim"), Some("nano")), ["nvim"]);
    }

    #[test]
    fn editor_is_used_when_visual_is_unset() {
        assert_eq!(editor_command(None, Some("nano")), ["nano"]);
    }

    #[test]
    fn neither_set_falls_back_to_an_editor_every_platform_has() {
        assert_eq!(editor_command(None, None), [FALLBACK_EDITOR]);
    }

    /// `EDITOR="code --wait"` is the common shape that a bare program name
    /// would try to exec as one long filename.
    #[test]
    fn arguments_in_the_value_are_kept_as_arguments() {
        assert_eq!(editor_command(Some("code --wait"), None), ["code", "--wait"]);
    }

    /// An exported but empty value names no editor, so it must not shadow the
    /// next choice.
    #[test]
    fn a_blank_value_is_treated_as_unset() {
        assert_eq!(editor_command(Some("   "), Some("nano")), ["nano"]);
        assert_eq!(editor_command(Some(""), None), [FALLBACK_EDITOR]);
    }
}
