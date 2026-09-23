//! Configuration and persisted state on disk.
//!
//! Two files with different owners: `config.yaml` belongs to the user, who is
//! expected to edit it by hand, while `state.json` is machine-owned and holds
//! things the user has no reason to edit — currently just the screencast
//! portal's restore token.
//!
//! The app writes `config.yaml` too, when settings are changed from the
//! interface, and that is why [`save_to`] goes to the trouble of splicing: the
//! file is the user's, comments and all, so a save edits the lines it has to and
//! leaves the rest alone.

mod splice;

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
    let config = load_unvalidated(path)?;

    config
        .validate()
        .with_context(|| format!("{} is not a usable configuration", path.display()))?;

    Ok(config)
}

/// Reads `path` without asking whether what it says can be run.
///
/// For an editor of the configuration rather than a user of it: a config with no
/// lights in it is exactly the one somebody needs to open, and refusing to load
/// it would leave them nothing to fix it with.
pub fn load_unvalidated(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    serde_yaml::from_str(&contents)
        .with_context(|| format!("Failed to parse {}; check its formatting", path.display()))
}

/// The commented example, as a configuration.
///
/// What a settings editor opens on a machine with no config file yet, so a first
/// run lands on the same sensible values the example carries rather than on an
/// error or a screen full of zeroes. The values still point at a broker that
/// does not exist, which is why this is not a default anything else falls back
/// to.
pub fn example() -> Result<Config> {
    serde_yaml::from_str(EXAMPLE_CONFIG).context("The built-in example configuration is broken")
}

/// The config path, with the commented example written there first if there is
/// nothing to open yet.
///
/// For `zync config`, which puts an editor on the file: a first run through there
/// should land in the same annotated example `load_or_init` would have created,
/// rather than an empty buffer. An existing file is never touched.
pub fn ensure_config() -> Result<PathBuf> {
    let path = config_path()?;

    match path.exists() {
        true => Ok(path),
        false => write_example(&path),
    }
}

fn write_example(path: &Path) -> Result<PathBuf> {
    // Running under sudo would write the example into root's home, where the
    // user will never find it, and the portal would refuse the session anyway.
    if std::env::var("USER").is_ok_and(|user| user == "root") {
        bail!("Don't run as root. Run as your normal user, without sudo.");
    }

    ensure_parent(path)?;
    fs::write(path, EXAMPLE_CONFIG)
        .with_context(|| format!("Failed to write {}", path.display()))?;

    Ok(path.to_path_buf())
}

/// Writes `config` to the standard path. Returns the path written.
pub fn save(config: &Config) -> Result<PathBuf> {
    let path = config_path()?;
    save_to(&path, config)?;

    Ok(path)
}

/// Writes `config` to `path`, preserving as much of the existing file as it can.
///
/// The file is the user's: it is the one they edit by hand, and both the example
/// and a config grown from it carry comments explaining what each field does. A
/// serde round-trip would drop all of them, so the new values are spliced into
/// the existing lines and everything else is copied through unchanged.
///
/// Two things are not preserved. Comments inside the `lights:` and `zones:`
/// blocks are lost whenever those change, because a list of mappings is rewritten
/// whole rather than field by field. And a file laid out in a style the splicer
/// does not understand is rewritten from scratch, with a warning, since a correct
/// config matters more than its formatting.
///
/// An invalid config is never written, and the write itself goes through a
/// temporary file so an interrupted save cannot leave half a config behind.
pub fn save_to(path: &Path, config: &Config) -> Result<()> {
    config.validate().with_context(|| {
        format!("Refusing to write an unusable configuration to {}", path.display())
    })?;

    // Every step is allowed to give up: an unreadable file, a layout the splicer
    // does not understand, and a splice that did not come back out as the config
    // asked for all land on the same full rendering.
    let spliced = fs::read_to_string(path)
        .ok()
        .and_then(|existing| match splice::splice(&existing, config) {
            Ok(text) => Some(text),
            Err(e) => {
                debug!(error = ?e, "config layout not understood");
                None
            }
        })
        .filter(|text| verify(text, config));

    let contents = match spliced {
        Some(text) => text,
        None => {
            if path.exists() {
                warn!(
                    path = %path.display(),
                    "could not preserve this file's layout; rewriting it from the configuration"
                );
            }
            render(config)?
        }
    };

    write_atomically(path, &contents)
}

/// Whether spliced text reads back as the config it was spliced from.
///
/// The splicer works on lines, so it can only ever be as good as its reading of
/// the file's shape. This is the check that makes that safe to rely on: text that
/// does not parse to exactly the config asked for is thrown away in favour of a
/// full rendering.
fn verify(text: &str, config: &Config) -> bool {
    match serde_yaml::from_str::<Config>(text) {
        Ok(parsed) => &parsed == config,
        Err(e) => {
            debug!(error = ?e, "spliced config does not parse");
            false
        }
    }
}

/// The whole config, rendered from scratch. Comments the file had are gone, hence
/// the pointer to where the documentation actually lives.
fn render(config: &Config) -> Result<String> {
    let body = serde_yaml::to_string(config).context("Failed to render the configuration")?;

    Ok(format!("{RENDERED_HEADER}{body}"))
}

/// Replaces `path` in one step, so that an interrupted write leaves either the
/// old config or the new one and never a truncated file. The temporary file is a
/// sibling because `rename` is only atomic within a filesystem.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    ensure_parent(path)?;
    let name = path
        .file_name()
        .context("Configuration path names no file")?
        .to_string_lossy();
    let temp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));

    fs::write(&temp, contents)
        .with_context(|| format!("Failed to write {}", temp.display()))?;
    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(e).with_context(|| format!("Failed to replace {}", path.display()));
    }

    Ok(())
}

/// Creates the directory a config file is about to land in. First run has no
/// `~/.config/zync` yet.
fn ensure_parent(path: &Path) -> Result<()> {
    let dir = path
        .parent()
        .context("Configuration path has no parent directory")?;

    fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))
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

/// Opens a config the app rendered itself, which is a config that has lost
/// whatever comments it used to carry.
const RENDERED_HEADER: &str = "\
# zync configuration, written by the app. The README documents every field:
# https://github.com/hendemic/zync

";

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
#       min_transition: 0.15   # fastest allowed transition, in seconds (Zigbee rounds to tenths; below 0.1 is an instant jump)
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
    use zync_core::domain::{
        Intensity, LightId, LightService, LightSpec, StopPolicy, TransitionCurve, Zone,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("zync-test-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }

        /// A config file holding `contents`, or no file at all for `None`.
        fn config(&self, contents: Option<&str>) -> PathBuf {
            let path = self.0.join(CONFIG_FILE);
            if let Some(contents) = contents {
                fs::write(&path, contents).expect("fixture");
            }

            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn example() -> Config {
        serde_yaml::from_str(EXAMPLE_CONFIG).expect("example must parse")
    }

    /// The pairs of lines that differ, which requires the two texts to have the
    /// same number of lines — itself the thing most of these tests are checking.
    fn differences<'a>(before: &'a str, after: &'a str) -> Vec<(&'a str, &'a str)> {
        assert_eq!(
            before.lines().count(),
            after.lines().count(),
            "the line count should not have changed"
        );

        before
            .lines()
            .zip(after.lines())
            .filter(|(before, after)| before != after)
            .collect()
    }

    fn comment_of(line: &str) -> Option<&str> {
        line.split_once('#').map(|(_, comment)| comment)
    }

    /// A config exercising every field the writer is allowed to leave out.
    fn every_option_set() -> Config {
        let mut config = example();
        config.instance = Some("gaming-rig".into());
        config.mqtt.user = Some("someone".into());
        config.mqtt.password = Some("hunter2".into());
        config.on_stop = StopPolicy::Off;
        config.intensity = Intensity::Custom(TransitionCurve {
            softness: 0.45,
            cut_midpoint: 0.5,
            cut_steepness: 12.0,
            min_transition: 0.05,
            max_transition: 1.2,
        });
        config.lights[0].max_updates_per_sec = Some(2.0);
        config.lights[0].fallback_state =
            Some(serde_json::json!({ "state": "ON", "brightness": 200 }));

        config
    }

    fn second_light() -> LightSpec {
        LightSpec {
            service: LightService::Zigbee2MQTT,
            light_name: LightId::new("desk_lamp"),
            brightness: 0.5,
            is_group: false,
            max_updates_per_sec: None,
            fallback_state: None,
        }
    }

    fn second_zone() -> Zone {
        Zone {
            name: "desk".into(),
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
            light_name: LightId::new("desk_lamp"),
        }
    }

    /// The whole point of splicing rather than re-rendering: a settings change
    /// touches the line it changes and nothing else.
    #[test]
    fn changing_one_scalar_rewrites_exactly_one_line() {
        /// A settings change, and how the line it edits should then read.
        type Case = (fn(&mut Config), &'static str);

        let cases: [Case; 4] = [
            (|config| config.performance.max_fps = 30, "max_fps: 30"),
            (|config| config.mqtt.broker = "10.0.0.5".into(), "broker: \"10.0.0.5\""),
            (|config| config.downsample_factor = 8, "downsample_factor: 8"),
            (|config| config.on_stop = StopPolicy::Hold, "on_stop: hold"),
        ];

        for (change, expected) in cases {
            let mut config = example();
            change(&mut config);

            let spliced = splice::splice(EXAMPLE_CONFIG, &config).expect("splice");
            let changed = differences(EXAMPLE_CONFIG, &spliced);

            assert_eq!(changed.len(), 1, "expected only `{expected}` to change, got {changed:?}");
            let (before, after) = changed[0];
            assert!(after.trim_start().starts_with(expected), "got {after:?}");
            assert_eq!(comment_of(after), comment_of(before), "the comment should survive");
            assert_eq!(serde_yaml::from_str::<Config>(&spliced).expect("parse"), config);
        }
    }

    /// Nothing changed means nothing written, down to the byte.
    #[test]
    fn saving_an_unchanged_config_rewrites_nothing() {
        assert_eq!(splice::splice(EXAMPLE_CONFIG, &example()).expect("splice"), EXAMPLE_CONFIG);
    }

    #[test]
    fn an_absent_top_level_key_is_added_and_removed_again() {
        let mut config = example();
        config.instance = Some("gaming-rig".into());

        let added = splice::splice(EXAMPLE_CONFIG, &config).expect("splice");

        assert!(added.ends_with("\ninstance: \"gaming-rig\"\n"), "got {added:?}");
        assert_eq!(serde_yaml::from_str::<Config>(&added).expect("parse"), config);
        // The commented-out example line is left as a comment rather than revived.
        assert!(added.contains("# instance: \"gaming-rig\""));

        assert_eq!(splice::splice(&added, &example()).expect("splice"), EXAMPLE_CONFIG);
    }

    #[test]
    fn an_absent_field_inside_a_block_is_removed_and_added_again() {
        let mut config = example();
        config.mqtt.password = None;

        let removed = splice::splice(EXAMPLE_CONFIG, &config).expect("splice");

        assert!(!removed.contains("password"), "got {removed}");
        assert_eq!(serde_yaml::from_str::<Config>(&removed).expect("parse"), config);
        assert_eq!(
            removed.lines().count(),
            EXAMPLE_CONFIG.lines().count() - 1,
            "one line should have gone"
        );

        config.mqtt.password = Some("hunter2".into());
        let added = splice::splice(&removed, &config).expect("splice");

        assert!(added.contains("\n  password: \"hunter2\"\n"), "got {added}");
        assert_eq!(serde_yaml::from_str::<Config>(&added).expect("parse"), config);
    }

    #[test]
    fn intensity_switches_between_a_preset_line_and_a_custom_block() {
        let mut config = example();
        config.intensity = Intensity::Custom(TransitionCurve {
            softness: 0.45,
            cut_midpoint: 0.5,
            cut_steepness: 12.0,
            min_transition: 0.05,
            max_transition: 1.2,
        });

        let custom = splice::splice(EXAMPLE_CONFIG, &config).expect("splice");

        assert!(custom.contains("\nintensity:\n  custom:\n    softness: 0.45\n"), "got {custom}");
        assert_eq!(serde_yaml::from_str::<Config>(&custom).expect("parse"), config);
        // The commentary above it explains both shapes, so it has to stay.
        assert!(custom.contains("# A custom curve is also accepted in place of a preset name:"));

        let preset = splice::splice(&custom, &example()).expect("splice");

        assert!(preset.contains("\nintensity: normal\n"), "got {preset}");
        assert_eq!(preset, EXAMPLE_CONFIG);
    }

    /// The two list blocks are rewritten whole, so what matters is that the
    /// rewrite stays inside them.
    #[test]
    fn replacing_the_lists_leaves_the_text_around_them_alone() {
        let mut config = example();
        config.lights.push(second_light());
        config.zones.push(second_zone());

        let spliced = splice::splice(EXAMPLE_CONFIG, &config).expect("splice");

        assert_eq!(serde_yaml::from_str::<Config>(&spliced).expect("parse"), config);

        let comments: Vec<&str> = EXAMPLE_CONFIG
            .lines()
            .skip_while(|line| !line.starts_with("# Zones are always given"))
            .take_while(|line| line.starts_with('#'))
            .collect();
        assert_eq!(comments.len(), 3, "fixture should have three lines of them");
        assert!(spliced.contains(&comments.join("\n")), "the comment above zones should survive");

        let (_, performance) = EXAMPLE_CONFIG.split_once("\nperformance:").expect("fixture");
        assert!(
            spliced.ends_with(&format!("\nperformance:{performance}")),
            "the performance block should be untouched"
        );
    }

    #[test]
    fn a_saved_config_loads_back_unchanged() {
        let dir = TempDir::new("save-round-trip");
        let path = dir.config(Some(EXAMPLE_CONFIG));

        // In order, so the second save splices onto the first one's output.
        for config in [example(), every_option_set(), example()] {
            save_to(&path, &config).expect("save");

            assert_eq!(load_from(&path).expect("load"), config);
        }
    }

    #[test]
    fn a_file_whose_layout_cannot_be_read_is_rewritten_whole() {
        let dir = TempDir::new("save-garbage");
        let path = dir.config(Some("mqtt: [this, is, not, a, mapping\n  nor: is this\n"));

        save_to(&path, &example()).expect("save");

        let written = fs::read_to_string(&path).expect("read");
        assert!(written.starts_with(RENDERED_HEADER), "got {written}");
        assert_eq!(load_from(&path).expect("load"), example());
    }

    #[test]
    fn a_missing_file_is_written_from_scratch() {
        let dir = TempDir::new("save-missing");
        let path = dir.config(None);

        save_to(&path, &every_option_set()).expect("save");

        assert!(fs::read_to_string(&path).expect("read").starts_with(RENDERED_HEADER));
        assert_eq!(load_from(&path).expect("load"), every_option_set());
    }

    /// The temporary file is how the write is made atomic; leaving one behind
    /// would drop a stray `config.yaml.<pid>.tmp` next to the real config.
    #[test]
    fn a_save_leaves_no_temporary_file_behind() {
        let dir = TempDir::new("save-temp");
        let path = dir.config(Some(EXAMPLE_CONFIG));
        let mut config = example();
        config.performance.max_fps = 20;

        save_to(&path, &config).expect("save");

        let leftovers: Vec<PathBuf> = fs::read_dir(&dir.0)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    /// A config that cannot run is a config that must not reach the disk, or the
    /// next start would fail on a file the user cannot see they broke.
    #[test]
    fn an_unusable_config_is_refused_rather_than_written() {
        let dir = TempDir::new("save-invalid");
        let path = dir.config(Some(EXAMPLE_CONFIG));
        let mut config = example();
        config.zones.clear();

        assert!(save_to(&path, &config).is_err());
        assert_eq!(fs::read_to_string(&path).expect("read"), EXAMPLE_CONFIG);
    }

    /// The example is the first thing a new user sees, and a typo in it produces
    /// a parse error on a file they did not write.
    #[test]
    fn the_example_config_parses_and_validates() {
        let config: Config = serde_yaml::from_str(EXAMPLE_CONFIG).expect("example must parse");

        assert!(config.validate().is_ok(), "example must be a usable configuration");
    }

    /// The README documents `custom:` as a nested map. serde_yaml's default enum
    /// encoding would demand a `!custom` tag instead, which nobody would guess.
    #[test]
    fn a_custom_intensity_parses_from_yaml_as_a_nested_map() {
        use zync_core::domain::Intensity;

        let yaml = "custom:\n  softness: 0.4\n  cut_midpoint: 0.4\n  cut_steepness: 14.0\n  min_transition: 0.1\n  max_transition: 1.0\n";
        let intensity: Intensity = serde_yaml::from_str(yaml).expect("nested map must parse");
        let Intensity::Custom(curve) = intensity else {
            panic!("expected a custom curve, got {intensity:?}");
        };
        assert_eq!(curve.min_transition, 0.1);

        let preset: Intensity = serde_yaml::from_str("extreme").expect("preset must parse");
        assert_eq!(preset, Intensity::Extreme);
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
