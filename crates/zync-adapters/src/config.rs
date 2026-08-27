//! Configuration and persisted state on disk.
//!
//! Two files with different owners: `config.yaml` is written by hand and only
//! ever read here, while `state.json` is machine-owned and holds things the user
//! has no reason to edit — currently just the screencast portal's restore token.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};
use zync_core::domain::Config;

const CONFIG_FILE: &str = "config.yaml";
const STATE_FILE: &str = "state.json";
const APP_DIR: &str = "zync";

/// Last resort when the system will not tell us its hostname.
const DEFAULT_INSTANCE: &str = "default";

/// Names this installation on the broker.
///
/// Two machines pointed at one broker must not share this. The MQTT client id has
/// to be unique per broker — MQTT 3.1.1 §3.1.4 requires the broker to disconnect
/// the older client when a new one connects with the same id, and since the event
/// loop reconnects, two instances sharing an id disconnect each other in a loop.
/// The control topic then decides which instance `zync stop` actually reaches.
///
/// Defaulting to the hostname means a config file copied to a second machine
/// works without being edited, which is the way this collision would otherwise
/// be met in practice.
pub fn resolve_instance(configured: Option<&str>) -> String {
    if let Some(name) = configured.map(sanitize_topic_level).filter(|n| !n.is_empty()) {
        return name;
    }

    hostname::get()
        .ok()
        .map(|host| sanitize_topic_level(&host.to_string_lossy()))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            warn!(
                "no instance name configured and the hostname is unavailable; using \
                 '{DEFAULT_INSTANCE}'. Set `instance:` in the config if more than one \
                 machine uses this broker."
            );
            DEFAULT_INSTANCE.to_string()
        })
}

/// Replaces what MQTT will not accept inside a single topic level. Hostnames are
/// normally already safe; a configured name is free text.
fn sanitize_topic_level(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| match c {
            '/' | '+' | '#' => '-',
            c if c.is_control() || c.is_whitespace() => '-',
            c => c,
        })
        .collect()
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("Could not locate a configuration directory for this user")?
        .join(APP_DIR))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

/// Machine-owned state. `state_dir` is absent on some platforms, so fall back to
/// the local data directory rather than dropping the file into the config.
fn state_dir() -> Result<PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("Could not locate a state directory for this user")?;

    Ok(base.join(APP_DIR))
}

pub fn state_path() -> Result<PathBuf> {
    Ok(state_dir()?.join(STATE_FILE))
}

pub fn log_dir() -> Result<PathBuf> {
    Ok(state_dir()?.join("logs"))
}

/// Records the running service so `status` and `stop` can find it without a
/// broker round-trip.
pub fn pid_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("zync.pid"))
}

/// Where a detached service's stdout and stderr go: whatever escapes the logger,
/// such as a panic.
///
/// Deliberately not named `zync.*`, because the rolling appender owns that
/// prefix and `zync logs` picks the newest file matching it.
pub fn stderr_path() -> Result<PathBuf> {
    Ok(log_dir()?.join("stderr.log"))
}

/// Loads the configuration, creating a commented example on first run.
///
/// Creating the example is a hard stop rather than a default: a generated config
/// points at a broker that does not exist, and silently carrying on with it looks
/// like a connection bug.
pub fn load_or_init() -> Result<Config> {
    let path = config_path()?;

    if !path.exists() {
        let created = write_example(&path)?;
        bail!("Config file created at {}\nEdit it and run again.", created.display());
    }

    load_from(&path)
}

pub fn load_from(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let config: Config = serde_yaml::from_str(&contents)
        .with_context(|| format!("Failed to parse {}; check its formatting", path.display()))?;

    config
        .validate()
        .with_context(|| format!("{} is not a usable configuration", path.display()))?;

    Ok(config)
}

fn write_example(path: &Path) -> Result<PathBuf> {
    // Running under sudo would write the example into root's home, where the
    // user will never find it, and the portal would refuse the session anyway.
    if std::env::var("USER").is_ok_and(|user| user == "root") {
        bail!("Don't run as root. Run as your normal user, without sudo.");
    }

    let dir = path
        .parent()
        .context("Configuration path has no parent directory")?;
    fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
    fs::write(path, EXAMPLE_CONFIG)
        .with_context(|| format!("Failed to write {}", path.display()))?;

    Ok(path.to_path_buf())
}

/// Persisted machine state. Best effort throughout: losing it costs the user one
/// extra portal prompt, so a read or write failure is never fatal.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct State {
    /// Lets the screencast portal hand back the same monitor without prompting.
    /// Without it, every start shows the source picker again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portal_restore_token: Option<String>,
}

impl State {
    pub fn load() -> Self {
        let Ok(path) = state_path() else {
            return State::default();
        };

        match fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|e| {
                debug!(error = ?e, path = %path.display(), "discarding unreadable state");
                State::default()
            }),
            Err(_) => State::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = state_path()?;
        let dir = path.parent().context("State path has no parent directory")?;
        fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
        fs::write(&path, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("Failed to write {}", path.display()))?;

        Ok(())
    }
}

const EXAMPLE_CONFIG: &str = r###"# Sample configuration: one light following a single zone covering a 1080p monitor.
# Enter your MQTT options, define lights, then set zones that map to those lights.
mqtt:
  name: "my-connection"
  broker: "192.168.1.100"
  port: 1883
  user: "user name"         # optional depending on broker config
  password: "password"      # optional depending on broker config

downsample_factor: 20       # pixel stride, in native display pixels

# Names this machine on the broker. Defaults to your hostname, which is usually
# what you want. Two machines pointed at the same broker must not share it: it
# sets both the MQTT client id and the topics `zync stop` uses, so a shared value
# means the two instances disconnect each other and `zync stop` may hit the wrong
# one. Only set this if you want a name other than the hostname.
# instance: "gaming-rig"

# What to do with the lights when syncing stops (zync stop, Ctrl-C, or a crash):
#   restore  put each light back the way it was before syncing started, falling
#            back to its fallback_state if that could not be read
#   default  always apply fallback_state
#   off      turn every light off
#   hold     leave the lights on the last colour they were sent
on_stop: restore

# How aggressively big colour jumps (cuts, explosions) are shortened relative to
# small, gradual changes:
#   slow     gentle fades throughout — good for film and ambient content
#   normal   the default balance (default if omitted)
#   extreme  snaps almost instantly on cuts — good for fast-paced games
# A custom curve is also accepted in place of a preset name:
#   intensity:
#     custom:
#       softness: 0.4          # falloff shape for small/gradual changes
#       cut_midpoint: 0.4      # normalized colour distance (0-1) where the cut kicks in
#       cut_steepness: 14.0    # how sharply transitions shorten past cut_midpoint
#       min_transition: 0.02   # fastest allowed transition, in seconds
#       max_transition: 1.0    # slowest allowed transition, in seconds
intensity: normal

lights:
  - light_name: "your_device_name"    # Must match the device name in Z2M. Can be a Z2M group or single light
    service: "Zigbee2MQTT"
    brightness: 0.8                   # percent brightness of light. range is 0-1. anything over 1 is rejected.
    is_group: false                   # set true for a Z2M group. Group commands are Zigbee broadcasts, which a mesh
                                      # only sustains at about 1/s, so groups are paced at 1 update/s (devices: 4/s).
    # max_updates_per_sec: 2          # optional override of that pacing for this light.

    # Used when on_stop is `default`, or when `restore` could not read this
    # light's previous state — which is common for groups. Anything the service
    # accepts works here; it is passed through untouched.
    # fallback_state:
    #   state: "ON"
    #   brightness: 200
    #   color_temp: 370

# Zones are always given in your display's native resolution. The app captures at
# a much smaller internal resolution for performance and converts these
# coordinates for you, so never scale them down yourself.
zones:
  - name: "main_screen"
    x: 0
    y: 0
    width: 1920
    height: 1080
    light_name: "your_device_name"  # Must match a light_name defined above

performance:
  max_fps: 12                       # max_fps. make sure it isn't too high for your lights. 10-12 is a safe starting point.
  max_delay: 500                    # max recovery delay in ms before retrying connection
  refresh_threshold: 10             # difference in color required to send MQTT light change
  percent_thread_work: 0.25         # max work/interval ratio.
  fps_reporting: 10                 # time in seconds between fps averages in the log. raise percent_thread_work for higher FPS.
  max_commands_per_sec: 6           # ceiling on light commands/sec across all zones.
                                    # Zigbee groups saturate well below the frame
                                    # rate; lower this if you see BUSY errors in Z2M.
"###;

#[cfg(test)]
mod tests {
    use super::*;

    /// The example is the first thing a new user sees, and a typo in it produces
    /// a parse error on a file they did not write.
    #[test]
    fn the_example_config_parses_and_validates() {
        let config: Config = serde_yaml::from_str(EXAMPLE_CONFIG).expect("example must parse");

        assert!(config.validate().is_ok(), "example must be a usable configuration");
    }

    #[test]
    fn a_configured_instance_name_wins() {
        assert_eq!(resolve_instance(Some("gaming-rig")), "gaming-rig");
    }

    /// An empty or blank name in the config must not become an empty topic level.
    #[test]
    fn a_blank_instance_name_falls_back_to_the_hostname() {
        let resolved = resolve_instance(Some("   "));

        assert!(!resolved.is_empty());
        assert_ne!(resolved.trim(), "");
    }

    #[test]
    fn an_absent_instance_name_resolves_to_something_usable() {
        let resolved = resolve_instance(None);

        assert!(!resolved.is_empty());
        assert!(!resolved.contains('/'));
    }

    /// A name carrying wildcards would silently widen or break the subscription
    /// it is spliced into, so those characters cannot survive.
    #[test]
    fn topic_wildcards_and_separators_are_replaced() {
        assert_eq!(resolve_instance(Some("a/b+c#d")), "a-b-c-d");
        assert_eq!(resolve_instance(Some("my desk")), "my-desk");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_rather_than_replaced() {
        assert_eq!(resolve_instance(Some("  desk  ")), "desk");
    }

    #[test]
    fn state_round_trips_through_json() {
        let state = State { portal_restore_token: Some("token-123".into()) };
        let encoded = serde_json::to_string(&state).unwrap();
        let decoded: State = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.portal_restore_token.as_deref(), Some("token-123"));
    }

    #[test]
    fn a_missing_state_file_reads_as_empty() {
        let decoded: State = serde_json::from_str("{}").unwrap();

        assert!(decoded.portal_restore_token.is_none());
    }
}
