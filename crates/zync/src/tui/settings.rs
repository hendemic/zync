//! The settings form: every value in the config, and what a key press does to it.
//!
//! Pure, like `app` next door. Loading and saving are asked for and performed
//! elsewhere; what happens here is that a config in memory changes.
//!
//! The rows are derived from the config rather than written out by the view, and
//! everything a row needs — its label, how it is changed, what it means — hangs
//! off [`Field`]. Adding a setting is then one variant and one arm in each of the
//! tables below, and nothing at all in the view.

use anyhow::{Error, Result};
use std::iter::successors;
use std::num::{IntErrorKind, ParseIntError};
use std::str::FromStr;
use zync_core::domain::{
    Config, ConfigError, Intensity, LightId, LightService, LightSpec, PerformanceConfig,
    StopPolicy, TransitionCurve, Zone,
};

use crate::ops::Saved;
use crate::tui::app::{Effect, Key, one_line};

/// Stands in for a password that is set. Long enough to read as a value and
/// short enough not to suggest a length.
const MASK: &str = "••••••";

/// What `a` puts in the Lights section. The name is meant to be typed over; the
/// brightness is the one the example config ships with.
const NEW_LIGHT_NAME: &str = "new_light";
const NEW_LIGHT_BRIGHTNESS: f32 = 0.8;

/// What `a` puts in the Zones section: one zone covering a 1080p screen, which
/// is where the example config starts too.
const NEW_ZONE_NAME: &str = "new_zone";
const NEW_ZONE_WIDTH: u32 = 1920;
const NEW_ZONE_HEIGHT: u32 = 1080;

/// One of the MQTT connection's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MqttField {
    Name,
    Broker,
    Port,
    User,
    Password,
}

/// One parameter of a hand-tuned intensity curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurveField {
    Softness,
    CutMidpoint,
    CutSteepness,
    MinTransition,
    MaxTransition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerfField {
    MaxFps,
    MaxDelay,
    RefreshThreshold,
    PercentThreadWork,
    FpsReporting,
    MaxCommandsPerSec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightField {
    Name,
    Brightness,
    IsGroup,
    MaxUpdates,
    Fallback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneField {
    Name,
    X,
    Y,
    Width,
    Height,
    Light,
}

/// A value in the config the form can change, named rather than reached by a
/// path so that a row, its label, its help and the cursor all speak of the same
/// thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Mqtt(MqttField),
    Instance,
    OnStop,
    Intensity,
    /// Only shown while `intensity` is custom.
    Curve(CurveField),
    Downsample,
    Perf(PerfField),
    Light(usize, LightField),
    Zone(usize, ZoneField),
}

/// A part of the config there can be any number of, and which the form can
/// therefore grow and shrink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Light,
    Zone,
}

impl Group {
    fn singular(self) -> &'static str {
        match self {
            Group::Light => "light",
            Group::Zone => "zone",
        }
    }
}

/// How a field is changed, which also decides how it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Typed, and always has a value.
    Text,
    /// Typed; an empty buffer means unset.
    OptionalText,
    /// Typed and unset when empty, but shown masked unless it is being typed.
    Secret,
    /// Typed; a buffer that does not parse is refused rather than committed.
    Number,
    /// A number that may be unset.
    OptionalNumber,
    /// Flipped rather than typed.
    Toggle,
    /// One of a fixed set, cycled with ←/→.
    Choice,
    /// Shown for the sake of completeness, changed in the file.
    ReadOnly,
}

impl Kind {
    /// Whether `Enter` opens a buffer on this field rather than acting at once.
    pub fn typed(self) -> bool {
        matches!(
            self,
            Kind::Text | Kind::OptionalText | Kind::Secret | Kind::Number | Kind::OptionalNumber
        )
    }

    /// Whether an empty buffer is a value in its own right.
    fn optional(self) -> bool {
        matches!(self, Kind::OptionalText | Kind::Secret | Kind::OptionalNumber)
    }
}

/// A line of the form. Headings are drawn but never landed on; the other two are
/// what the cursor walks between.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Heading { text: String, indent: usize },
    Field { field: Field, indent: usize },
    /// Grows a section by one. A row of its own so that a config with no lights
    /// in it — exactly the config this form exists to fix — still has somewhere
    /// to put the cursor.
    Add { group: Group, indent: usize },
}

impl Row {
    pub fn indent(&self) -> usize {
        match self {
            Row::Heading { indent, .. } | Row::Field { indent, .. } | Row::Add { indent, .. } => {
                *indent
            }
        }
    }

    fn selectable(&self) -> bool {
        !matches!(self, Row::Heading { .. })
    }

    fn field(&self) -> Option<Field> {
        match self {
            Row::Field { field, .. } => Some(*field),
            _ => None,
        }
    }
}

/// A value being typed: which field it belongs to, what has been typed so far,
/// and where in it the next character lands.
pub struct Edit {
    pub field: Field,
    pub buffer: String,
    /// Counted in characters rather than bytes, so that moving across one is
    /// always one step whatever it is made of.
    pub cursor: usize,
}

impl Edit {
    /// The buffer in three pieces: what is before the cursor, the character
    /// under it, and what follows. The character under the cursor is empty at
    /// the end of the buffer.
    ///
    /// Split here rather than in the view because where a character starts and
    /// ends is this type's business, and the interface hides the terminal's own
    /// cursor, so the only cursor there is is the one drawn.
    pub fn split(&self) -> (&str, &str, &str) {
        let at = offset(&self.buffer, self.cursor);
        let rest = &self.buffer[at..];
        let width = rest.chars().next().map_or(0, char::len_utf8);

        (&self.buffer[..at], &rest[..width], &rest[width..])
    }
}

/// A key press that has been asked to confirm itself. Held for exactly one
/// press: anything that is not the answer cancels it, so a stray key can never
/// discard an edit or delete a light.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Leave,
    Open,
    Delete,
}

/// What a key press asks the screen around the form to do. Everything else a key
/// does, it does to the config in place.
#[derive(Debug, PartialEq)]
pub enum Action {
    Do(Effect),
    /// Close the form and go back to the menu.
    Leave,
}

/// The form's whole state.
pub struct Settings {
    /// The config as edited. `None` until a load lands, and after one that
    /// failed.
    config: Option<Config>,
    /// What was loaded, or last saved. What "unsaved changes" means.
    baseline: Option<Config>,
    rows: Vec<Row>,
    cursor: usize,
    /// Index of the topmost visible row.
    scroll: usize,
    /// Rows the last frame had room for. Written by the view, because the height
    /// of a frame is the one thing about scrolling that only the frame knows.
    height: usize,
    edit: Option<Edit>,
    pending: Option<Pending>,
    /// The line at the foot of the screen: what just happened.
    pub note: String,
    /// Whether `note` reports something that went wrong.
    pub problem: bool,
    /// Why there is nothing to edit. Set only when the file could not be read at
    /// all, which leaves `o` and `q` as the only two useful keys.
    pub load_error: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            config: None,
            baseline: None,
            rows: Vec::new(),
            cursor: 0,
            scroll: 0,
            height: 0,
            edit: None,
            pending: None,
            note: "Loading the config…".to_owned(),
            problem: false,
            load_error: None,
        }
    }
}

impl Settings {
    /// Forgets the last visit. The load that follows fills the form in, and
    /// nothing from before it is worth carrying over — least of all a message
    /// about a save that has scrolled out of anyone's memory.
    pub fn opening(&mut self) {
        *self = Settings::default();
    }

    /// Takes the config the event loop read for us.
    pub fn loaded(&mut self, outcome: Result<Config>) {
        match outcome {
            Ok(config) => {
                self.rows = rows_for(&config);
                self.baseline = Some(config.clone());
                self.config = Some(config);
                self.load_error = None;
                self.cursor = 0;
                self.scroll = 0;
                self.settle(1);
                self.say(String::new());
            }
            // A config that will not parse cannot be shown as fields, so the
            // form says why and offers the file itself instead.
            Err(e) => {
                self.config = None;
                self.baseline = None;
                self.rows.clear();
                self.load_error = Some(one_line(&e));
                self.fail("Press o to open the file in your editor, or q to go back.");
            }
        }
    }

    /// Takes the result of the save the event loop performed.
    pub fn saved(&mut self, outcome: Result<Saved>) {
        match outcome {
            Ok(saved) => {
                // What was written is what unsaved changes are now measured
                // against, so saving twice in a row is a no-op the second time.
                self.baseline = self.config.clone();
                self.say(match saved.restart_needed {
                    true => format!(
                        "Saved to {}; zync is running, so it takes effect on the next start.",
                        saved.path.display()
                    ),
                    false => format!("Saved to {}.", saved.path.display()),
                });
            }
            Err(e) => self.fail(one_line(&e)),
        }
    }

    /// Reports something that went wrong around the form rather than in it.
    pub fn failed(&mut self, error: &Error) {
        self.fail(one_line(error));
    }

    /// Whether the form holds anything the file does not.
    pub fn dirty(&self) -> bool {
        self.config.is_some() && self.config != self.baseline
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// The value being typed, when one is.
    pub fn editing(&self) -> Option<&Edit> {
        self.edit.as_ref()
    }

    /// Told by the view how many rows it got. A shorter window must not leave
    /// the cursor off the screen, which is the whole reason this is not just a
    /// field.
    pub fn resize(&mut self, height: usize) {
        self.height = height;
        self.follow();
    }

    /// The field under the cursor, when the cursor is on one.
    pub fn selected_field(&self) -> Option<Field> {
        self.rows.get(self.cursor).and_then(Row::field)
    }

    /// The one-line explanation of whatever the cursor is on.
    pub fn help(&self) -> &'static str {
        match self.rows.get(self.cursor) {
            Some(Row::Field { field, .. }) => help(*field),
            Some(Row::Add { group, .. }) => match group {
                Group::Light => "Adds a light with defaults to edit. d deletes the one you are on.",
                Group::Zone => "Adds a zone covering a 1080p screen. d deletes the one you are on.",
            },
            _ => "",
        }
    }

    /// The label a row is drawn under.
    pub fn label(&self, field: Field) -> &'static str {
        label(field)
    }

    pub fn kind(&self, field: Field) -> Kind {
        kind(field)
    }

    /// What a row shows for a field: the value, made readable. A password is
    /// masked here, and an unset value says so rather than leaving a blank that
    /// reads as an empty string.
    pub fn display(&self, field: Field) -> String {
        let Some(config) = &self.config else {
            return String::new();
        };

        match field {
            Field::Mqtt(MqttField::Password) => match config.mqtt.password {
                Some(_) => MASK.to_owned(),
                None => unset(),
            },
            Field::Light(index, LightField::Fallback) => match config.lights.get(index) {
                Some(light) if light.fallback_state.is_some() => "set".to_owned(),
                _ => "not set".to_owned(),
            },
            Field::Light(index, LightField::IsGroup) => match config.lights.get(index) {
                Some(light) if light.is_group => "yes".to_owned(),
                _ => "no".to_owned(),
            },
            // A zone pointing at a light that is not configured is the one
            // mistake this screen can show before a save tries it.
            Field::Zone(index, ZoneField::Light) => match config.zones.get(index) {
                Some(zone) if config.light(&zone.light_name).is_some() => {
                    zone.light_name.to_string()
                }
                Some(zone) if zone.light_name.as_str().is_empty() => "no light chosen".to_owned(),
                Some(zone) => format!("{} — no such light", zone.light_name),
                None => String::new(),
            },
            _ => match (self.text(field), kind(field).optional()) {
                (text, true) if text.is_empty() => unset(),
                (text, _) => text,
            },
        }
    }

    /// The label for a row that grows a section.
    pub fn add_label(&self, group: Group) -> String {
        format!("+ add a {}", group.singular())
    }

    /// The value as it goes into an edit buffer: what is really there, with no
    /// masking and no standing in for the unset.
    pub fn text(&self, field: Field) -> String {
        let Some(config) = &self.config else {
            return String::new();
        };
        let light = |index: usize| config.lights.get(index);
        let zone = |index: usize| config.zones.get(index);

        match field {
            Field::Mqtt(MqttField::Name) => config.mqtt.name.clone(),
            Field::Mqtt(MqttField::Broker) => config.mqtt.broker.clone(),
            Field::Mqtt(MqttField::Port) => config.mqtt.port.to_string(),
            Field::Mqtt(MqttField::User) => config.mqtt.user.clone().unwrap_or_default(),
            Field::Mqtt(MqttField::Password) => config.mqtt.password.clone().unwrap_or_default(),
            Field::Instance => config.instance.clone().unwrap_or_default(),
            Field::OnStop => policy_name(config.on_stop).to_owned(),
            Field::Intensity => intensity_name(config.intensity).to_owned(),
            Field::Curve(which) => number(curve_value(config.intensity.curve(), which)),
            Field::Downsample => config.downsample_factor.to_string(),
            Field::Perf(which) => perf_value(&config.performance, which),
            Field::Light(index, which) => light(index).map_or_else(String::new, |light| {
                match which {
                    LightField::Name => light.light_name.to_string(),
                    LightField::Brightness => number(light.brightness),
                    LightField::IsGroup => light.is_group.to_string(),
                    LightField::MaxUpdates => light.max_updates_per_sec.map(number).unwrap_or_default(),
                    LightField::Fallback => String::new(),
                }
            }),
            Field::Zone(index, which) => zone(index).map_or_else(String::new, |zone| match which {
                ZoneField::Name => zone.name.clone(),
                ZoneField::X => zone.x.to_string(),
                ZoneField::Y => zone.y.to_string(),
                ZoneField::Width => zone.width.to_string(),
                ZoneField::Height => zone.height.to_string(),
                ZoneField::Light => zone.light_name.to_string(),
            }),
        }
    }

    pub fn on_key(&mut self, key: Key) -> Option<Action> {
        // A buffer swallows every letter, which is what lets a broker be called
        // "sadq" without the hotkeys firing four times.
        if self.edit.is_some() {
            self.typing(key);
            return None;
        }

        let confirming = self.pending.take();

        // A prompt that has been answered, either way, stops standing. Whatever
        // asks again puts its own message back.
        if confirming.is_some() {
            self.say(String::new());
        }

        if self.load_error.is_some() {
            return match key {
                Key::Char('o') => Some(Action::Do(Effect::Edit)),
                Key::Char('q') | Key::Esc => Some(Action::Leave),
                _ => None,
            };
        }

        match key {
            // j and k are movement before they are letters, as on the home
            // screen.
            Key::Up | Key::Char('k') => self.step(-1),
            Key::Down | Key::Char('j') => self.step(1),
            Key::PageUp => self.page(-1),
            Key::PageDown => self.page(1),
            Key::Home => self.jump(-1),
            Key::End => self.jump(1),
            Key::Left => self.nudge(-1),
            Key::Right => self.nudge(1),
            Key::Enter | Key::Char(' ') => return self.activate(),
            Key::Char('a') => self.add(self.group_at_cursor()),
            Key::Char('d') => self.delete(confirming),
            Key::Char('s') => return self.save(),
            Key::Char('o') => return self.open_file(confirming),
            Key::Char('q') | Key::Esc => return self.leave(confirming),
            _ => {}
        }

        None
    }

    /// Acts on the row under the cursor: opens a buffer on a typed field, flips
    /// a toggle, cycles a choice, grows a section.
    fn activate(&mut self) -> Option<Action> {
        match self.rows.get(self.cursor) {
            Some(Row::Add { group, .. }) => self.add(Some(*group)),
            Some(Row::Field { field, .. }) => {
                let field = *field;
                match kind(field) {
                    kind if kind.typed() => self.begin(field),
                    Kind::Toggle | Kind::Choice => self.cycle(field, 1),
                    _ => self.say("This one is edited in the file; press o to open it."),
                }
            }
            _ => {}
        }

        None
    }

    /// ←/→ on a toggle or a choice. Typed fields are left alone: a stray arrow
    /// key should not change a value that has to be committed to change.
    fn nudge(&mut self, delta: isize) {
        if let Some(field) = self.selected_field()
            && matches!(kind(field), Kind::Toggle | Kind::Choice)
        {
            self.cycle(field, delta);
        }
    }

    fn typing(&mut self, key: Key) {
        let Some(edit) = self.edit.as_mut() else {
            return;
        };

        match key {
            Key::Char(typed) => {
                let at = offset(&edit.buffer, edit.cursor);
                edit.buffer.insert(at, typed);
                edit.cursor += 1;
            }
            Key::Backspace if edit.cursor > 0 => {
                edit.cursor -= 1;
                let at = offset(&edit.buffer, edit.cursor);
                edit.buffer.remove(at);
            }
            Key::Left => edit.cursor = edit.cursor.saturating_sub(1),
            Key::Right => edit.cursor = (edit.cursor + 1).min(edit.buffer.chars().count()),
            Key::Home => edit.cursor = 0,
            Key::End => edit.cursor = edit.buffer.chars().count(),
            Key::Enter => self.commit(),
            Key::Esc => {
                self.edit = None;
                self.say(String::new());
            }
            _ => {}
        }
    }

    fn begin(&mut self, field: Field) {
        let buffer = self.text(field);
        let cursor = buffer.chars().count();

        self.edit = Some(Edit { field, buffer, cursor });
        self.say(String::new());
    }

    /// Writes the buffer back, or keeps it open with the reason it could not be.
    ///
    /// Staying in edit mode is the point: a mistyped number is a character out
    /// of place, and throwing the whole value away to report that would be worse
    /// than the mistake.
    fn commit(&mut self) {
        let Some(edit) = self.edit.take() else {
            return;
        };

        // A password is taken exactly as typed. Everything else is trimmed,
        // because a space on the end of a broker address is never meant.
        let text = match kind(edit.field) {
            Kind::Secret => edit.buffer.clone(),
            _ => edit.buffer.trim().to_owned(),
        };

        match self.write(edit.field, &text) {
            Ok(()) => {
                self.say(String::new());
                self.rebuild(Some(edit.field));
            }
            Err(problem) => {
                self.edit = Some(edit);
                self.fail(problem);
            }
        }
    }

    /// The one place a typed value reaches the config.
    fn write(&mut self, field: Field, text: &str) -> Result<(), String> {
        let intensity = self.config.as_ref().map(|config| config.intensity);
        let Some(config) = self.config.as_mut() else {
            return Ok(());
        };

        match field {
            Field::Mqtt(MqttField::Name) => config.mqtt.name = text.to_owned(),
            Field::Mqtt(MqttField::Broker) => config.mqtt.broker = text.to_owned(),
            Field::Mqtt(MqttField::Port) => config.mqtt.port = whole(text)?,
            Field::Mqtt(MqttField::User) => config.mqtt.user = optional(text),
            Field::Mqtt(MqttField::Password) => config.mqtt.password = optional(text),
            Field::Instance => config.instance = optional(text),
            Field::Downsample => config.downsample_factor = whole(text)?,
            Field::Curve(which) => {
                // Only reachable while the intensity is custom, and a curve is
                // edited one number at a time, so the other four come back out
                // of whatever is there now.
                let mut curve = intensity.unwrap_or_default().curve();
                set_curve_value(&mut curve, which, decimal(text)?);
                config.intensity = Intensity::Custom(curve);
            }
            Field::Perf(which) => set_perf_value(&mut config.performance, which, text)?,
            Field::Light(index, which) => {
                let light = config
                    .lights
                    .get_mut(index)
                    .ok_or_else(|| "That light is no longer there.".to_owned())?;

                match which {
                    LightField::Name => light.light_name = LightId::new(text),
                    LightField::Brightness => light.brightness = decimal(text)?,
                    LightField::MaxUpdates => {
                        light.max_updates_per_sec = match text.is_empty() {
                            true => None,
                            false => Some(decimal(text)?),
                        }
                    }
                    LightField::IsGroup | LightField::Fallback => {}
                }
            }
            Field::Zone(index, which) => {
                let zone = config
                    .zones
                    .get_mut(index)
                    .ok_or_else(|| "That zone is no longer there.".to_owned())?;

                match which {
                    ZoneField::Name => zone.name = text.to_owned(),
                    ZoneField::X => zone.x = whole(text)?,
                    ZoneField::Y => zone.y = whole(text)?,
                    ZoneField::Width => zone.width = whole(text)?,
                    ZoneField::Height => zone.height = whole(text)?,
                    ZoneField::Light => zone.light_name = LightId::new(text),
                }
            }
            Field::OnStop | Field::Intensity => {}
        }

        Ok(())
    }

    /// Moves a toggle or a choice on by `delta`, wrapping at both ends.
    fn cycle(&mut self, field: Field, delta: isize) {
        let names: Vec<String> = match field {
            Field::Zone(..) => self.light_names(),
            _ => Vec::new(),
        };
        let Some(config) = self.config.as_mut() else {
            return;
        };

        match field {
            Field::OnStop => {
                let at = POLICIES.iter().position(|p| *p == config.on_stop);
                config.on_stop = POLICIES[next_choice(at, POLICIES.len(), delta)];
            }
            Field::Intensity => {
                let at = PRESETS.iter().position(|p| *p == config.intensity);
                // Custom sits past the presets, and arrives carrying the curve
                // of whichever preset was showing — so the five numbers that
                // appear are the ones that were in force a moment ago.
                let next = next_choice(
                    at.or(Some(PRESETS.len())),
                    PRESETS.len() + 1,
                    delta,
                );
                config.intensity = match PRESETS.get(next) {
                    Some(preset) => *preset,
                    None => Intensity::Custom(config.intensity.curve()),
                };
            }
            Field::Light(index, LightField::IsGroup) => {
                if let Some(light) = config.lights.get_mut(index) {
                    light.is_group = !light.is_group;
                }
            }
            Field::Zone(index, ZoneField::Light) => {
                if names.is_empty() {
                    self.fail("There are no lights to point this zone at yet.");
                    return;
                }
                if let Some(zone) = config.zones.get_mut(index) {
                    let at = names.iter().position(|name| *name == zone.light_name.to_string());
                    zone.light_name = LightId::new(&names[next_choice(at, names.len(), delta)]);
                }
            }
            _ => return,
        }

        // A value that has just moved answers whatever the last message
        // complained about, so the complaint goes with it.
        self.say(String::new());
        self.rebuild(Some(field));
    }

    /// Adds a light or a zone, and puts the cursor on its first field so it can
    /// be named straight away.
    fn add(&mut self, group: Option<Group>) {
        let Some(group) = group else {
            self.say("Press a inside Lights or Zones to add one.");
            return;
        };
        let first_light = self
            .config
            .as_ref()
            .and_then(|config| config.lights.first())
            .map(|light| light.light_name.clone());
        let Some(config) = self.config.as_mut() else {
            return;
        };

        let anchor = match group {
            Group::Light => {
                config.lights.push(LightSpec {
                    // The only service with an implementation behind it, so the
                    // form neither asks nor shows it.
                    service: LightService::Zigbee2MQTT,
                    light_name: LightId::new(NEW_LIGHT_NAME),
                    brightness: NEW_LIGHT_BRIGHTNESS,
                    is_group: false,
                    max_updates_per_sec: None,
                    fallback_state: None,
                });
                Field::Light(config.lights.len() - 1, LightField::Name)
            }
            Group::Zone => {
                config.zones.push(Zone {
                    name: NEW_ZONE_NAME.to_owned(),
                    x: 0,
                    y: 0,
                    width: NEW_ZONE_WIDTH,
                    height: NEW_ZONE_HEIGHT,
                    light_name: first_light.unwrap_or_else(|| LightId::new("")),
                });
                Field::Zone(config.zones.len() - 1, ZoneField::Name)
            }
        };

        self.rebuild(Some(anchor));
        self.say(format!("Added a {}.", group.singular()));
    }

    /// Deletes the light or zone under the cursor, once it has been asked twice.
    fn delete(&mut self, confirming: Option<Pending>) {
        let Some((group, index)) = self.selected_field().and_then(|field| match field {
            Field::Light(index, _) => Some((Group::Light, index)),
            Field::Zone(index, _) => Some((Group::Zone, index)),
            _ => None,
        }) else {
            self.say("Press d on a light or a zone to delete it.");
            return;
        };

        if confirming != Some(Pending::Delete) {
            self.pending = Some(Pending::Delete);
            self.fail(format!(
                "Press d again to delete this {}.",
                group.singular()
            ));
            return;
        }

        let Some(config) = self.config.as_mut() else {
            return;
        };

        // The cursor stays where it was, on whatever moved up into the gap, and
        // falls back to the row that would add another one.
        let anchor = match group {
            Group::Light => {
                config.lights.remove(index);
                let next = index.min(config.lights.len().saturating_sub(1));
                (!config.lights.is_empty()).then_some(Field::Light(next, LightField::Name))
            }
            Group::Zone => {
                config.zones.remove(index);
                let next = index.min(config.zones.len().saturating_sub(1));
                (!config.zones.is_empty()).then_some(Field::Zone(next, ZoneField::Name))
            }
        };

        self.rebuild(anchor);
        if anchor.is_none() {
            self.point_at_add(group);
        }
        self.say(format!("Deleted the {}.", group.singular()));
    }

    /// Validates first, so a config that cannot be run is refused here — where
    /// the cursor can be moved to the field that is wrong — rather than by the
    /// writer, which can only say so.
    fn save(&mut self) -> Option<Action> {
        let config = self.config.clone()?;

        match config.validate() {
            Ok(()) => {
                self.say("Saving…");
                Some(Action::Do(Effect::SaveSettings(Box::new(config))))
            }
            Err(e) => {
                self.point_at(&e);
                self.fail(e.to_string());
                None
            }
        }
    }

    /// Hands the file to the external editor, which is still the only way to
    /// reach what this form does not show.
    fn open_file(&mut self, confirming: Option<Pending>) -> Option<Action> {
        if self.dirty() && confirming != Some(Pending::Open) {
            self.pending = Some(Pending::Open);
            self.fail("Unsaved changes — press o again to discard them and open the file, or s to save.");
            return None;
        }

        Some(Action::Do(Effect::Edit))
    }

    fn leave(&mut self, confirming: Option<Pending>) -> Option<Action> {
        if self.dirty() && confirming != Some(Pending::Leave) {
            self.pending = Some(Pending::Leave);
            self.fail("Unsaved changes — press q again to discard them, or s to save.");
            return None;
        }

        Some(Action::Leave)
    }

    /// Puts the cursor on the field a validation error is about, so that the
    /// message at the foot and the row under the cursor say the same thing.
    fn point_at(&mut self, error: &ConfigError) {
        let row = match error {
            ConfigError::NoLights => self.row_of_add(Group::Light),
            ConfigError::NoZones => self.row_of_add(Group::Zone),
            ConfigError::BrightnessOutOfRange { light, .. } => self
                .index_of_light(light)
                .and_then(|index| self.row_of(Field::Light(index, LightField::Brightness))),
            ConfigError::UnknownLight { zone, .. } => self
                .index_of_zone(zone)
                .and_then(|index| self.row_of(Field::Zone(index, ZoneField::Light))),
            ConfigError::CurveMidpointOutOfRange { .. } => {
                self.row_of(Field::Curve(CurveField::CutMidpoint))
            }
            ConfigError::CurveSteepnessNotPositive { .. } => {
                self.row_of(Field::Curve(CurveField::CutSteepness))
            }
            ConfigError::CurveMinTransitionNegative { .. }
            | ConfigError::CurveMinExceedsMax { .. } => {
                self.row_of(Field::Curve(CurveField::MinTransition))
            }
        };

        if let Some(index) = row {
            self.cursor = index;
            self.follow();
        }
    }

    fn point_at_add(&mut self, group: Group) {
        if let Some(index) = self.row_of_add(group) {
            self.cursor = index;
            self.follow();
        }
    }

    /// Redraws the row list after a change, keeping the cursor on `anchor` where
    /// that row still exists. Row counts move whenever a light is added or the
    /// intensity turns custom, so this is what keeps the cursor on the value the
    /// user was looking at rather than on the index it happened to have.
    fn rebuild(&mut self, anchor: Option<Field>) {
        let Some(config) = &self.config else {
            return;
        };

        self.rows = rows_for(config);
        if let Some(index) = anchor.and_then(|field| self.row_of(field)) {
            self.cursor = index;
        }
        self.settle(1);
    }

    /// Puts the cursor back on a row that can hold it, looking `direction` first.
    fn settle(&mut self, direction: isize) {
        if self.rows.is_empty() {
            self.cursor = 0;
            return;
        }

        self.cursor = self.cursor.min(self.rows.len() - 1);
        if !self.rows[self.cursor].selectable() {
            let found = self
                .seek(self.cursor, direction)
                .or_else(|| self.seek(self.cursor, -direction));

            if let Some(index) = found {
                self.cursor = index;
            }
        }

        self.follow();
    }

    /// The next selectable row from `from` in `direction`, if there is one.
    fn seek(&self, from: usize, direction: isize) -> Option<usize> {
        successors(Some(from as isize), |index| Some(index + direction))
            .skip(1)
            .take_while(|index| (0..self.rows.len() as isize).contains(index))
            .find(|index| self.rows[*index as usize].selectable())
            .map(|index| index as usize)
    }

    fn step(&mut self, delta: isize) {
        if let Some(index) = self.seek(self.cursor, delta) {
            self.cursor = index;
            self.follow();
        }
    }

    /// A screenful, and never nothing, so a frame that has not been drawn yet
    /// still moves.
    fn page(&mut self, delta: isize) {
        let last = self.rows.len().saturating_sub(1) as isize;
        let target = self.cursor as isize + delta * self.height.max(1) as isize;

        self.cursor = target.clamp(0, last) as usize;
        self.settle(delta);
    }

    fn jump(&mut self, direction: isize) {
        self.cursor = match direction < 0 {
            true => 0,
            false => self.rows.len().saturating_sub(1),
        };
        self.settle(-direction);
    }

    /// Scrolls just enough to keep the cursor on the screen.
    fn follow(&mut self) {
        if self.height == 0 {
            return;
        }

        // A field reads better with the heading it belongs to above it, so the
        // top of the window reaches one row further back when that row is a
        // heading. The lower bound still wins, so the cursor is on the screen
        // either way.
        let context = match self.cursor.checked_sub(1) {
            Some(above) if matches!(self.rows.get(above), Some(Row::Heading { .. })) => above,
            _ => self.cursor,
        };

        self.scroll = self
            .scroll
            .min(context)
            .max((self.cursor + 1).saturating_sub(self.height))
            .min(self.rows.len().saturating_sub(self.height));
    }

    fn row_of(&self, field: Field) -> Option<usize> {
        self.rows.iter().position(|row| row.field() == Some(field))
    }

    fn row_of_add(&self, group: Group) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(row, Row::Add { group: at, .. } if *at == group))
    }

    /// Which section the cursor is standing in, when it is in one that can grow.
    fn group_at_cursor(&self) -> Option<Group> {
        match self.rows.get(self.cursor) {
            Some(Row::Add { group, .. }) => Some(*group),
            Some(Row::Field { field: Field::Light(..), .. }) => Some(Group::Light),
            Some(Row::Field { field: Field::Zone(..), .. }) => Some(Group::Zone),
            _ => None,
        }
    }

    fn light_names(&self) -> Vec<String> {
        self.config
            .iter()
            .flat_map(|config| config.lights.iter())
            .map(|light| light.light_name.to_string())
            .collect()
    }

    fn index_of_light(&self, id: &LightId) -> Option<usize> {
        self.config
            .as_ref()?
            .lights
            .iter()
            .position(|light| &light.light_name == id)
    }

    fn index_of_zone(&self, name: &str) -> Option<usize> {
        self.config
            .as_ref()?
            .zones
            .iter()
            .position(|zone| zone.name == name)
    }

    fn say(&mut self, text: impl Into<String>) {
        self.note = text.into();
        self.problem = false;
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.note = text.into();
        self.problem = true;
    }
}

const MQTT_FIELDS: [MqttField; 5] = [
    MqttField::Name,
    MqttField::Broker,
    MqttField::Port,
    MqttField::User,
    MqttField::Password,
];

const CURVE_FIELDS: [CurveField; 5] = [
    CurveField::Softness,
    CurveField::CutMidpoint,
    CurveField::CutSteepness,
    CurveField::MinTransition,
    CurveField::MaxTransition,
];

const PERF_FIELDS: [PerfField; 6] = [
    PerfField::MaxFps,
    PerfField::MaxDelay,
    PerfField::RefreshThreshold,
    PerfField::PercentThreadWork,
    PerfField::FpsReporting,
    PerfField::MaxCommandsPerSec,
];

const LIGHT_FIELDS: [LightField; 5] = [
    LightField::Name,
    LightField::Brightness,
    LightField::IsGroup,
    LightField::MaxUpdates,
    LightField::Fallback,
];

const ZONE_FIELDS: [ZoneField; 6] = [
    ZoneField::Name,
    ZoneField::X,
    ZoneField::Y,
    ZoneField::Width,
    ZoneField::Height,
    ZoneField::Light,
];

/// The choices `on_stop` cycles through, in the order the README lists them.
const POLICIES: [StopPolicy; 4] = [
    StopPolicy::Restore,
    StopPolicy::Default,
    StopPolicy::Off,
    StopPolicy::Hold,
];

/// The intensity presets, gentlest first. Custom follows them, and is not here
/// because it carries a curve rather than being a value on its own.
const PRESETS: [Intensity; 3] = [Intensity::Slow, Intensity::Normal, Intensity::Extreme];

/// The whole form, in the order it is drawn.
///
/// Built from the config every time it changes, because what there is to show
/// depends on it: five curve fields appear with a custom intensity, and a light
/// or a zone brings its own group of rows with it.
fn rows_for(config: &Config) -> Vec<Row> {
    let connection = [heading("Connection", 0)]
        .into_iter()
        .chain(MQTT_FIELDS.map(|which| field(Field::Mqtt(which), 1)));

    let curve = matches!(config.intensity, Intensity::Custom(_))
        .then(|| CURVE_FIELDS.map(|which| field(Field::Curve(which), 2)))
        .into_iter()
        .flatten();

    let behaviour = [
        heading("Behaviour", 0),
        field(Field::Instance, 1),
        field(Field::OnStop, 1),
        field(Field::Intensity, 1),
    ]
    .into_iter()
    .chain(curve)
    .chain([field(Field::Downsample, 1)]);

    let performance = [heading("Performance", 0)]
        .into_iter()
        .chain(PERF_FIELDS.map(|which| field(Field::Perf(which), 1)));

    let lights = [heading("Lights", 0)]
        .into_iter()
        .chain(config.lights.iter().enumerate().flat_map(|(index, light)| {
            [heading(light.light_name.as_str(), 1)]
                .into_iter()
                .chain(LIGHT_FIELDS.map(|which| field(Field::Light(index, which), 2)))
        }))
        .chain([Row::Add { group: Group::Light, indent: 1 }]);

    let zones = [heading("Zones", 0)]
        .into_iter()
        .chain(config.zones.iter().enumerate().flat_map(|(index, zone)| {
            [heading(&zone.name, 1)]
                .into_iter()
                .chain(ZONE_FIELDS.map(|which| field(Field::Zone(index, which), 2)))
        }))
        .chain([Row::Add { group: Group::Zone, indent: 1 }]);

    connection
        .chain(behaviour)
        .chain(performance)
        .chain(lights)
        .chain(zones)
        .collect()
}

fn heading(text: &str, indent: usize) -> Row {
    Row::Heading { text: text.to_owned(), indent }
}

fn field(field: Field, indent: usize) -> Row {
    Row::Field { field, indent }
}

/// The name a field is drawn under, which is the name it has in the file. The
/// form and the YAML then describe the same thing in the same words.
fn label(field: Field) -> &'static str {
    match field {
        Field::Mqtt(MqttField::Name) => "name",
        Field::Mqtt(MqttField::Broker) => "broker",
        Field::Mqtt(MqttField::Port) => "port",
        Field::Mqtt(MqttField::User) => "user",
        Field::Mqtt(MqttField::Password) => "password",
        Field::Instance => "instance",
        Field::OnStop => "on_stop",
        Field::Intensity => "intensity",
        Field::Curve(CurveField::Softness) => "softness",
        Field::Curve(CurveField::CutMidpoint) => "cut_midpoint",
        Field::Curve(CurveField::CutSteepness) => "cut_steepness",
        Field::Curve(CurveField::MinTransition) => "min_transition",
        Field::Curve(CurveField::MaxTransition) => "max_transition",
        Field::Downsample => "downsample_factor",
        Field::Perf(PerfField::MaxFps) => "max_fps",
        Field::Perf(PerfField::MaxDelay) => "max_delay",
        Field::Perf(PerfField::RefreshThreshold) => "refresh_threshold",
        Field::Perf(PerfField::PercentThreadWork) => "percent_thread_work",
        Field::Perf(PerfField::FpsReporting) => "fps_reporting",
        Field::Perf(PerfField::MaxCommandsPerSec) => "max_commands_per_sec",
        Field::Light(_, LightField::Name) => "light_name",
        Field::Light(_, LightField::Brightness) => "brightness",
        Field::Light(_, LightField::IsGroup) => "is_group",
        Field::Light(_, LightField::MaxUpdates) => "max_updates_per_sec",
        Field::Light(_, LightField::Fallback) => "fallback_state",
        Field::Zone(_, ZoneField::Name) => "name",
        Field::Zone(_, ZoneField::X) => "x",
        Field::Zone(_, ZoneField::Y) => "y",
        Field::Zone(_, ZoneField::Width) => "width",
        Field::Zone(_, ZoneField::Height) => "height",
        Field::Zone(_, ZoneField::Light) => "light_name",
    }
}

fn kind(field: Field) -> Kind {
    match field {
        Field::Mqtt(MqttField::Name | MqttField::Broker) => Kind::Text,
        Field::Mqtt(MqttField::Port) => Kind::Number,
        Field::Mqtt(MqttField::User) => Kind::OptionalText,
        Field::Mqtt(MqttField::Password) => Kind::Secret,
        Field::Instance => Kind::OptionalText,
        Field::OnStop | Field::Intensity => Kind::Choice,
        Field::Curve(_) | Field::Downsample | Field::Perf(_) => Kind::Number,
        Field::Light(_, LightField::Name) => Kind::Text,
        Field::Light(_, LightField::Brightness) => Kind::Number,
        Field::Light(_, LightField::IsGroup) => Kind::Toggle,
        Field::Light(_, LightField::MaxUpdates) => Kind::OptionalNumber,
        Field::Light(_, LightField::Fallback) => Kind::ReadOnly,
        Field::Zone(_, ZoneField::Name) => Kind::Text,
        Field::Zone(_, ZoneField::Light) => Kind::Choice,
        Field::Zone(_, _) => Kind::Number,
    }
}

/// One sentence per field, drawn under the form for whichever the cursor is on.
/// The source is the commented example config, which is where anyone editing the
/// file by hand reads the same thing.
fn help(field: Field) -> &'static str {
    match field {
        Field::Mqtt(MqttField::Name) => {
            "Names this connection on the broker; it prefixes the MQTT client id."
        }
        Field::Mqtt(MqttField::Broker) => "Hostname or address of your MQTT broker.",
        Field::Mqtt(MqttField::Port) => "Port the broker listens on; 1883 is the usual one.",
        Field::Mqtt(MqttField::User) => "Username, if the broker asks for one. Leave it empty if it does not.",
        Field::Mqtt(MqttField::Password) => "Password for that user. Leave it empty if the broker asks for none.",
        Field::Instance => {
            "Names this machine on the broker; defaults to the hostname. Two machines on one broker must not share it."
        }
        Field::OnStop => {
            "What the lights do when syncing stops: restore what they were, apply their fallback state, turn off, or hold."
        }
        Field::Intensity => {
            "How hard big colour jumps are shortened: slow for film, extreme for games, custom for a hand-tuned curve."
        }
        Field::Curve(CurveField::Softness) => {
            "Falloff shape for small, gradual changes. Lower is snappier."
        }
        Field::Curve(CurveField::CutMidpoint) => {
            "Colour distance, 0 to 1, at which a change counts as a cut and its fade is shortened."
        }
        Field::Curve(CurveField::CutSteepness) => {
            "How sharply fades shorten either side of the midpoint. Higher is snappier."
        }
        Field::Curve(CurveField::MinTransition) => {
            "Fastest fade, in seconds. Zigbee rounds to tenths, and below 0.1 becomes an instant jump."
        }
        Field::Curve(CurveField::MaxTransition) => {
            "Slowest fade, in seconds; also the fade a first sample gets."
        }
        Field::Downsample => {
            "Pixel stride, in native display pixels: how coarsely each zone is sampled."
        }
        Field::Perf(PerfField::MaxFps) => {
            "Ceiling on frames sampled per second. 10 to 12 is a safe start for most lights."
        }
        Field::Perf(PerfField::MaxDelay) => {
            "Longest wait, in milliseconds, before a lost connection is retried."
        }
        Field::Perf(PerfField::RefreshThreshold) => {
            "How far a zone's colour must move before the light is told about it."
        }
        Field::Perf(PerfField::PercentThreadWork) => {
            "Share of each frame's time capture may use before the frame rate is throttled."
        }
        Field::Perf(PerfField::FpsReporting) => "Seconds between frame-rate averages in the log.",
        Field::Perf(PerfField::MaxCommandsPerSec) => {
            "Ceiling on light commands per second across every zone. Lower it if Zigbee2MQTT reports BUSY."
        }
        Field::Light(_, LightField::Name) => {
            "The device or group name exactly as Zigbee2MQTT knows it."
        }
        Field::Light(_, LightField::Brightness) => {
            "How bright this light goes, from 0.0 to 1.0."
        }
        Field::Light(_, LightField::IsGroup) => {
            "Whether this name is a Zigbee2MQTT group rather than one device. Groups are paced far more slowly."
        }
        Field::Light(_, LightField::MaxUpdates) => {
            "Overrides the pacing: 1 update a second for a group, 4 for a device. Empty keeps that default."
        }
        Field::Light(_, LightField::Fallback) => {
            "Where this light is left when its own state could not be read. Edit this in the file."
        }
        Field::Zone(_, ZoneField::Name) => "A name for this zone; it appears in the logs.",
        Field::Zone(_, ZoneField::X) => "Left edge of the zone, in native display pixels.",
        Field::Zone(_, ZoneField::Y) => "Top edge of the zone, in native display pixels.",
        Field::Zone(_, ZoneField::Width) => "Width of the zone, in native display pixels.",
        Field::Zone(_, ZoneField::Height) => "Height of the zone, in native display pixels.",
        Field::Zone(_, ZoneField::Light) => "Which of the configured lights follows this zone.",
    }
}

fn policy_name(policy: StopPolicy) -> &'static str {
    match policy {
        StopPolicy::Restore => "restore",
        StopPolicy::Default => "default",
        StopPolicy::Off => "off",
        StopPolicy::Hold => "hold",
    }
}

fn intensity_name(intensity: Intensity) -> &'static str {
    match intensity {
        Intensity::Slow => "slow",
        Intensity::Normal => "normal",
        Intensity::Extreme => "extreme",
        Intensity::Custom(_) => "custom",
    }
}

fn curve_value(curve: TransitionCurve, which: CurveField) -> f32 {
    match which {
        CurveField::Softness => curve.softness,
        CurveField::CutMidpoint => curve.cut_midpoint,
        CurveField::CutSteepness => curve.cut_steepness,
        CurveField::MinTransition => curve.min_transition,
        CurveField::MaxTransition => curve.max_transition,
    }
}

fn set_curve_value(curve: &mut TransitionCurve, which: CurveField, value: f32) {
    match which {
        CurveField::Softness => curve.softness = value,
        CurveField::CutMidpoint => curve.cut_midpoint = value,
        CurveField::CutSteepness => curve.cut_steepness = value,
        CurveField::MinTransition => curve.min_transition = value,
        CurveField::MaxTransition => curve.max_transition = value,
    }
}

fn perf_value(performance: &PerformanceConfig, which: PerfField) -> String {
    match which {
        PerfField::MaxFps => performance.max_fps.to_string(),
        PerfField::MaxDelay => performance.max_delay.to_string(),
        PerfField::RefreshThreshold => performance.refresh_threshold.to_string(),
        PerfField::PercentThreadWork => number(performance.percent_thread_work),
        PerfField::FpsReporting => performance.fps_reporting.to_string(),
        PerfField::MaxCommandsPerSec => number(performance.max_commands_per_sec),
    }
}

fn set_perf_value(
    performance: &mut PerformanceConfig,
    which: PerfField,
    text: &str,
) -> Result<(), String> {
    match which {
        PerfField::MaxFps => performance.max_fps = whole(text)?,
        PerfField::MaxDelay => performance.max_delay = whole(text)?,
        PerfField::RefreshThreshold => performance.refresh_threshold = whole(text)?,
        PerfField::PercentThreadWork => performance.percent_thread_work = decimal(text)?,
        PerfField::FpsReporting => performance.fps_reporting = whole(text)?,
        PerfField::MaxCommandsPerSec => performance.max_commands_per_sec = decimal(text)?,
    }

    Ok(())
}

/// What an unset optional value shows as. A blank would read as an empty string,
/// which for a username is a different thing.
fn unset() -> String {
    "(unset)".to_owned()
}

/// A float, as short as it can be written. `to_string` already drops a trailing
/// zero, which keeps `0.8` from being drawn back as `0.80000001`.
fn number(value: f32) -> String {
    value.to_string()
}

fn optional(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_owned())
}

/// The next index in a list of choices, wrapping at both ends. A value that is
/// in no list joins it at whichever end the move came from.
fn next_choice(current: Option<usize>, len: usize, delta: isize) -> usize {
    match current {
        Some(index) => (index as isize + delta).rem_euclid(len as isize) as usize,
        None if delta < 0 => len.saturating_sub(1),
        None => 0,
    }
}

/// Byte offset of the character at `index`, or the end of the string.
fn offset(text: &str, index: usize) -> usize {
    text.char_indices().nth(index).map_or(text.len(), |(at, _)| at)
}

/// A whole number, with the two ways of not being one told apart: a value that
/// is not a number at all and one the field has no room for want different
/// things done about them.
fn whole<T: FromStr<Err = ParseIntError>>(text: &str) -> Result<T, String> {
    text.parse().map_err(|e: ParseIntError| match e.kind() {
        IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => {
            format!("{text} is outside the range this setting allows.")
        }
        IntErrorKind::Empty => "This setting needs a whole number.".to_owned(),
        _ => format!("{text:?} is not a whole number."),
    })
}

fn decimal(text: &str) -> Result<f32, String> {
    text.parse()
        .map_err(|_| format!("{text:?} is not a number."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use std::path::PathBuf;
    use zync_core::domain::MqttConfig;

    /// Two lights and two zones, which is enough surface for the delete and
    /// light-picker cases without every test having to build its own.
    fn config() -> Config {
        Config {
            mqtt: MqttConfig {
                name: "t".into(),
                broker: "localhost".into(),
                port: 1883,
                user: None,
                password: Some("hunter2".into()),
            },
            lights: vec![
                LightSpec {
                    service: LightService::Zigbee2MQTT,
                    light_name: LightId::new("lamp1"),
                    brightness: 0.8,
                    is_group: false,
                    max_updates_per_sec: None,
                    fallback_state: None,
                },
                LightSpec {
                    service: LightService::Zigbee2MQTT,
                    light_name: LightId::new("lamp2"),
                    brightness: 0.6,
                    is_group: false,
                    max_updates_per_sec: None,
                    fallback_state: None,
                },
            ],
            zones: vec![
                Zone {
                    name: "zone1".into(),
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    light_name: LightId::new("lamp1"),
                },
                Zone {
                    name: "zone2".into(),
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    light_name: LightId::new("lamp2"),
                },
            ],
            downsample_factor: 1,
            performance: PerformanceConfig {
                max_fps: 60,
                max_delay: 500,
                refresh_threshold: 10,
                percent_thread_work: 0.25,
                fps_reporting: 3600,
                max_commands_per_sec: 20.0,
            },
            on_stop: StopPolicy::Restore,
            instance: None,
            intensity: Intensity::Normal,
        }
    }

    fn loaded_settings() -> Settings {
        let mut s = Settings::default();
        s.loaded(Ok(config()));
        s
    }

    fn place(s: &mut Settings, field: Field) {
        s.cursor = s.row_of(field).expect("field is on the form");
    }

    // --- Loading and navigation ---------------------------------------

    #[test]
    fn loading_lands_the_cursor_on_the_first_field() {
        let s = loaded_settings();
        assert_eq!(s.selected_field(), Some(Field::Mqtt(MqttField::Name)));
    }

    #[test]
    fn up_and_down_skip_headings() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Password));

        s.on_key(Key::Down);
        assert_eq!(s.selected_field(), Some(Field::Instance));

        s.on_key(Key::Up);
        assert_eq!(s.selected_field(), Some(Field::Mqtt(MqttField::Password)));
    }

    #[test]
    fn j_and_k_skip_headings_like_the_arrow_keys() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Password));

        s.on_key(Key::Char('j'));
        assert_eq!(s.selected_field(), Some(Field::Instance));

        s.on_key(Key::Char('k'));
        assert_eq!(s.selected_field(), Some(Field::Mqtt(MqttField::Password)));
    }

    #[test]
    fn home_and_end_jump_to_the_first_and_last_selectable_rows() {
        let mut s = loaded_settings();
        place(&mut s, Field::Instance);

        s.on_key(Key::Home);
        assert_eq!(s.selected_field(), Some(Field::Mqtt(MqttField::Name)));

        s.on_key(Key::End);
        assert!(matches!(&s.rows()[s.cursor()], Row::Add { group: Group::Zone, .. }));
    }

    #[test]
    fn page_down_moves_by_the_window_height_and_never_lands_on_a_heading() {
        let mut s = loaded_settings();
        s.resize(5);

        s.on_key(Key::PageDown);

        // Cursor starts on mqtt.name (row 1); five rows down lands on the
        // Behaviour heading, which settle() pushes forward onto instance.
        assert_eq!(s.selected_field(), Some(Field::Instance));
        assert!(!matches!(&s.rows()[s.cursor()], Row::Heading { .. }));
    }

    #[test]
    fn resize_to_a_small_height_keeps_the_cursor_on_screen() {
        let mut s = loaded_settings();
        place(&mut s, Field::Perf(PerfField::MaxCommandsPerSec));

        s.resize(3);

        assert!(s.scroll() <= s.cursor());
        assert!(s.cursor() < s.scroll() + 3);
    }

    // --- Text edit -------------------------------------------------------

    #[test]
    fn enter_on_broker_opens_a_buffer_with_the_cursor_at_the_end() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));

        s.on_key(Key::Enter);

        let edit = s.editing().unwrap();
        assert_eq!(edit.field, Field::Mqtt(MqttField::Broker));
        assert_eq!(edit.buffer, "localhost");
        assert_eq!(edit.cursor, "localhost".chars().count());
    }

    #[test]
    fn hotkey_letters_are_inserted_rather_than_acted_on_while_editing() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));
        s.on_key(Key::Enter);

        for ch in ['s', 'a', 'd', 'q', 'j', 'k', 'o'] {
            assert_eq!(s.on_key(Key::Char(ch)), None);
        }

        assert!(s.editing().is_some());
        assert_eq!(s.editing().unwrap().buffer, "localhostsadqjko");
        assert_eq!(s.text(Field::Mqtt(MqttField::Broker)), "localhost");
    }

    #[test]
    fn backspace_removes_the_character_before_the_cursor() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));
        s.on_key(Key::Enter);

        s.on_key(Key::Backspace);

        assert_eq!(s.editing().unwrap().buffer, "localhos");
    }

    #[test]
    fn left_and_right_move_the_edit_cursor() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));
        s.on_key(Key::Enter);

        s.on_key(Key::Left);
        s.on_key(Key::Left);
        assert_eq!(s.editing().unwrap().cursor, "localhost".chars().count() - 2);

        s.on_key(Key::Right);
        assert_eq!(s.editing().unwrap().cursor, "localhost".chars().count() - 1);
    }

    #[test]
    fn home_and_end_move_the_edit_cursor_to_the_ends_of_the_buffer() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));
        s.on_key(Key::Enter);

        s.on_key(Key::Home);
        assert_eq!(s.editing().unwrap().cursor, 0);

        s.on_key(Key::End);
        assert_eq!(s.editing().unwrap().cursor, "localhost".chars().count());
    }

    #[test]
    fn enter_commits_the_buffer_into_the_config() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));
        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "example.com".to_owned();

        s.on_key(Key::Enter);

        assert!(s.editing().is_none());
        assert_eq!(s.text(Field::Mqtt(MqttField::Broker)), "example.com");
    }

    #[test]
    fn esc_cancels_the_edit_and_leaves_the_config_unchanged() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));
        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "example.com".to_owned();

        s.on_key(Key::Esc);

        assert!(s.editing().is_none());
        assert_eq!(s.text(Field::Mqtt(MqttField::Broker)), "localhost");
    }

    // --- Number edit -------------------------------------------------------

    #[test]
    fn committing_a_non_numeric_port_stays_in_edit_mode_with_a_problem() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Port));
        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "abc".to_owned();

        s.on_key(Key::Enter);

        assert!(s.editing().is_some());
        assert!(s.problem);
        assert!(s.note.contains("not a whole number"));
    }

    #[test]
    fn committing_a_port_above_u16_range_reports_out_of_range() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Port));
        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "70000".to_owned();

        s.on_key(Key::Enter);

        assert!(s.editing().is_some());
        assert!(s.problem);
        assert!(s.note.contains("outside the range"));
    }

    #[test]
    fn committing_a_valid_port_updates_the_config() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Port));
        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "1884".to_owned();

        s.on_key(Key::Enter);

        assert!(s.editing().is_none());
        assert!(!s.problem);
        assert_eq!(s.text(Field::Mqtt(MqttField::Port)), "1884");
    }

    #[test]
    fn a_float_field_accepts_a_decimal_and_rejects_non_numeric_text() {
        let mut s = loaded_settings();
        place(&mut s, Field::Perf(PerfField::PercentThreadWork));

        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "0.5".to_owned();
        s.on_key(Key::Enter);
        assert!(s.editing().is_none());
        assert_eq!(s.text(Field::Perf(PerfField::PercentThreadWork)), "0.5");

        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "x".to_owned();
        s.on_key(Key::Enter);
        assert!(s.editing().is_some());
        assert!(s.problem);
    }

    // --- Optional fields -----------------------------------------------

    #[test]
    fn committing_an_empty_buffer_on_an_optional_text_field_unsets_it() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::User));
        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = String::new();

        s.on_key(Key::Enter);

        assert_eq!(s.text(Field::Mqtt(MqttField::User)), "");
        assert_eq!(s.display(Field::Mqtt(MqttField::User)), "(unset)");
    }

    #[test]
    fn instance_round_trips_through_being_set_and_cleared() {
        let mut s = loaded_settings();
        place(&mut s, Field::Instance);
        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "porch".to_owned();
        s.on_key(Key::Enter);
        assert_eq!(s.text(Field::Instance), "porch");

        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = String::new();
        s.on_key(Key::Enter);

        assert_eq!(s.display(Field::Instance), "(unset)");
    }

    // --- Secret --------------------------------------------------------

    #[test]
    fn a_set_password_is_masked_in_display_but_not_in_its_edit_buffer() {
        let s = loaded_settings();

        assert_eq!(s.display(Field::Mqtt(MqttField::Password)), MASK);
        assert_eq!(s.text(Field::Mqtt(MqttField::Password)), "hunter2");
    }

    // --- Choices ---------------------------------------------------------

    #[test]
    fn left_and_right_cycle_on_stop_through_all_policies_and_wrap() {
        let mut s = loaded_settings();
        place(&mut s, Field::OnStop);
        assert_eq!(s.text(Field::OnStop), "restore");

        s.on_key(Key::Right);
        assert_eq!(s.text(Field::OnStop), "default");
        s.on_key(Key::Right);
        assert_eq!(s.text(Field::OnStop), "off");
        s.on_key(Key::Right);
        assert_eq!(s.text(Field::OnStop), "hold");
        s.on_key(Key::Right);
        assert_eq!(s.text(Field::OnStop), "restore");

        s.on_key(Key::Left);
        assert_eq!(s.text(Field::OnStop), "hold");
    }

    #[test]
    fn enter_and_space_cycle_a_choice_forward() {
        let mut s = loaded_settings();
        place(&mut s, Field::OnStop);

        s.on_key(Key::Enter);
        assert_eq!(s.text(Field::OnStop), "default");

        s.on_key(Key::Char(' '));
        assert_eq!(s.text(Field::OnStop), "off");
    }

    #[test]
    fn left_and_right_do_nothing_on_a_text_field() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));

        s.on_key(Key::Right);
        s.on_key(Key::Left);

        assert_eq!(s.text(Field::Mqtt(MqttField::Broker)), "localhost");
        assert!(s.editing().is_none());
    }

    // --- Intensity ---------------------------------------------------------

    #[test]
    fn cycling_past_extreme_turns_the_intensity_custom_with_extremes_curve() {
        let mut s = loaded_settings();
        place(&mut s, Field::Intensity);

        s.on_key(Key::Right); // normal -> extreme
        assert_eq!(s.text(Field::Intensity), "extreme");
        s.on_key(Key::Right); // extreme -> custom
        assert_eq!(s.text(Field::Intensity), "custom");

        let index = s.row_of(Field::Intensity).unwrap();
        let curve_fields: Vec<Field> =
            (index + 1..index + 6).map(|i| s.rows()[i].field().unwrap()).collect();
        assert_eq!(curve_fields, CURVE_FIELDS.map(Field::Curve).to_vec());

        let expected = Intensity::Extreme.curve();
        for which in CURVE_FIELDS {
            assert_eq!(s.text(Field::Curve(which)), number(curve_value(expected, which)));
        }
    }

    #[test]
    fn editing_a_curve_value_changes_only_that_number() {
        let mut s = loaded_settings();
        place(&mut s, Field::Intensity);
        s.on_key(Key::Right); // extreme
        s.on_key(Key::Right); // custom, seeded from extreme's curve
        let before = Intensity::Extreme.curve();

        place(&mut s, Field::Curve(CurveField::MinTransition));
        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "0.3".to_owned();
        s.on_key(Key::Enter);

        assert_eq!(s.text(Field::Curve(CurveField::MinTransition)), "0.3");
        for which in
            [CurveField::Softness, CurveField::CutMidpoint, CurveField::CutSteepness, CurveField::MaxTransition]
        {
            assert_eq!(s.text(Field::Curve(which)), number(curve_value(before, which)));
        }
    }

    #[test]
    fn cycling_back_to_a_preset_removes_the_curve_rows() {
        let mut s = loaded_settings();
        place(&mut s, Field::Intensity);
        s.on_key(Key::Right); // extreme
        s.on_key(Key::Right); // custom
        assert!(s.row_of(Field::Curve(CurveField::Softness)).is_some());

        s.on_key(Key::Left); // back to extreme

        assert_eq!(s.text(Field::Intensity), "extreme");
        assert!(s.row_of(Field::Curve(CurveField::Softness)).is_none());
    }

    // --- Toggle ----------------------------------------------------------

    #[test]
    fn enter_on_is_group_flips_it() {
        let mut s = loaded_settings();
        place(&mut s, Field::Light(0, LightField::IsGroup));
        assert_eq!(s.text(Field::Light(0, LightField::IsGroup)), "false");

        s.on_key(Key::Enter);

        assert_eq!(s.text(Field::Light(0, LightField::IsGroup)), "true");
    }

    // --- Zone light picker -------------------------------------------------

    #[test]
    fn left_and_right_cycle_a_zones_light_through_the_configured_lights() {
        let mut s = loaded_settings();
        place(&mut s, Field::Zone(0, ZoneField::Light));
        assert_eq!(s.display(Field::Zone(0, ZoneField::Light)), "lamp1");

        s.on_key(Key::Right);
        assert_eq!(s.display(Field::Zone(0, ZoneField::Light)), "lamp2");
        s.on_key(Key::Right);
        assert_eq!(s.display(Field::Zone(0, ZoneField::Light)), "lamp1");

        s.on_key(Key::Left);
        assert_eq!(s.display(Field::Zone(0, ZoneField::Light)), "lamp2");
    }

    #[test]
    fn a_zone_pointing_at_a_missing_light_says_so_and_cycling_lands_on_a_real_one() {
        let mut c = config();
        c.zones[0].light_name = LightId::new("ghost");
        let mut s = Settings::default();
        s.loaded(Ok(c));
        place(&mut s, Field::Zone(0, ZoneField::Light));
        assert_eq!(s.display(Field::Zone(0, ZoneField::Light)), "ghost — no such light");

        s.on_key(Key::Right);

        assert_eq!(s.display(Field::Zone(0, ZoneField::Light)), "lamp1");
    }

    #[test]
    fn cycling_a_zones_light_with_no_lights_configured_reports_a_problem() {
        let mut c = config();
        c.lights.clear();
        let mut s = Settings::default();
        s.loaded(Ok(c));
        place(&mut s, Field::Zone(0, ZoneField::Light));

        s.on_key(Key::Right);

        assert!(s.problem);
        assert!(s.note.contains("no lights"));
    }

    // --- Add -------------------------------------------------------------

    #[test]
    fn a_on_a_light_row_appends_a_new_light_and_selects_its_name() {
        let mut s = loaded_settings();
        place(&mut s, Field::Light(0, LightField::Name));

        s.on_key(Key::Char('a'));

        assert_eq!(s.selected_field(), Some(Field::Light(2, LightField::Name)));
        assert_eq!(s.text(Field::Light(2, LightField::Name)), NEW_LIGHT_NAME);
    }

    #[test]
    fn a_on_the_add_a_zone_row_appends_a_zone_pointing_at_the_first_light() {
        let mut s = loaded_settings();
        s.cursor = s.row_of_add(Group::Zone).unwrap();

        s.on_key(Key::Char('a'));

        assert_eq!(s.selected_field(), Some(Field::Zone(2, ZoneField::Name)));
        assert_eq!(s.display(Field::Zone(2, ZoneField::Light)), "lamp1");
    }

    #[test]
    fn enter_on_the_add_a_zone_row_also_appends_a_zone() {
        let mut s = loaded_settings();
        s.cursor = s.row_of_add(Group::Zone).unwrap();

        s.on_key(Key::Enter);

        assert_eq!(s.selected_field(), Some(Field::Zone(2, ZoneField::Name)));
    }

    #[test]
    fn a_outside_lights_or_zones_only_sets_a_note() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));

        s.on_key(Key::Char('a'));

        assert_eq!(s.text(Field::Mqtt(MqttField::Broker)), "localhost");
        assert!(s.row_of(Field::Light(2, LightField::Name)).is_none());
        assert!(s.note.contains("Press a inside"));
    }

    // --- Delete ----------------------------------------------------------

    #[test]
    fn d_once_asks_for_confirmation_without_deleting() {
        let mut s = loaded_settings();
        place(&mut s, Field::Light(0, LightField::Name));

        s.on_key(Key::Char('d'));

        assert!(s.problem);
        assert!(s.note.contains("Press d again"));
        assert_eq!(s.text(Field::Light(0, LightField::Name)), "lamp1");
        assert!(s.row_of(Field::Light(1, LightField::Name)).is_some());
    }

    #[test]
    fn d_twice_deletes_the_light_under_the_cursor() {
        let mut s = loaded_settings();
        place(&mut s, Field::Light(0, LightField::Name));

        s.on_key(Key::Char('d'));
        s.on_key(Key::Char('d'));

        assert_eq!(s.text(Field::Light(0, LightField::Name)), "lamp2");
        assert!(s.row_of(Field::Light(1, LightField::Name)).is_none());
    }

    #[test]
    fn a_different_key_between_the_two_ds_cancels_the_delete() {
        let mut s = loaded_settings();
        place(&mut s, Field::Light(0, LightField::Name));

        s.on_key(Key::Char('d'));
        s.on_key(Key::Down); // still within the same light's fields
        s.on_key(Key::Char('d')); // asks again rather than deleting

        assert!(s.problem);
        assert!(s.note.contains("Press d again"));
        assert_eq!(s.text(Field::Light(0, LightField::Name)), "lamp1");
        assert!(s.row_of(Field::Light(1, LightField::Name)).is_some());
    }

    #[test]
    fn deleting_the_last_zone_leaves_the_cursor_on_add_a_zone() {
        let mut c = config();
        c.zones.remove(1);
        let mut s = Settings::default();
        s.loaded(Ok(c));
        place(&mut s, Field::Zone(0, ZoneField::Name));

        s.on_key(Key::Char('d'));
        s.on_key(Key::Char('d'));

        assert!(matches!(&s.rows()[s.cursor()], Row::Add { group: Group::Zone, .. }));
    }

    // --- Dirty and leave ---------------------------------------------------

    #[test]
    fn a_freshly_loaded_config_is_not_dirty() {
        assert!(!loaded_settings().dirty());
    }

    fn edit_instance_to_porch(s: &mut Settings) {
        place(s, Field::Instance);
        s.on_key(Key::Enter);
        s.edit.as_mut().unwrap().buffer = "porch".to_owned();
        s.on_key(Key::Enter);
    }

    #[test]
    fn an_edited_config_is_dirty() {
        let mut s = loaded_settings();
        edit_instance_to_porch(&mut s);

        assert!(s.dirty());
    }

    #[test]
    fn q_when_dirty_asks_for_confirmation_before_leaving() {
        let mut s = loaded_settings();
        edit_instance_to_porch(&mut s);

        let action = s.on_key(Key::Char('q'));

        assert_eq!(action, None);
        assert!(s.note.contains("Unsaved changes"));
    }

    #[test]
    fn a_second_q_leaves_discarding_the_changes() {
        let mut s = loaded_settings();
        edit_instance_to_porch(&mut s);
        s.on_key(Key::Char('q'));

        let action = s.on_key(Key::Char('q'));

        assert_eq!(action, Some(Action::Leave));
    }

    #[test]
    fn a_different_key_between_two_qs_cancels_the_pending_leave() {
        let mut s = loaded_settings();
        edit_instance_to_porch(&mut s);
        s.on_key(Key::Char('q'));
        s.on_key(Key::Down);

        let action = s.on_key(Key::Char('q'));

        assert_eq!(action, None);
        assert!(s.note.contains("Unsaved changes"));
    }

    #[test]
    fn q_leaves_at_once_when_there_are_no_unsaved_changes() {
        let mut s = loaded_settings();

        assert_eq!(s.on_key(Key::Char('q')), Some(Action::Leave));
    }

    #[test]
    fn o_when_dirty_asks_for_confirmation_before_opening_the_file() {
        let mut s = loaded_settings();
        edit_instance_to_porch(&mut s);

        let action = s.on_key(Key::Char('o'));

        assert_eq!(action, None);
        assert!(s.note.contains("Unsaved changes"));
    }

    #[test]
    fn a_second_o_opens_the_file_discarding_the_changes() {
        let mut s = loaded_settings();
        edit_instance_to_porch(&mut s);
        s.on_key(Key::Char('o'));

        let action = s.on_key(Key::Char('o'));

        assert_eq!(action, Some(Action::Do(Effect::Edit)));
    }

    #[test]
    fn o_opens_at_once_when_there_are_no_unsaved_changes() {
        let mut s = loaded_settings();

        assert_eq!(s.on_key(Key::Char('o')), Some(Action::Do(Effect::Edit)));
    }

    // --- Save ------------------------------------------------------------

    #[test]
    fn s_on_a_valid_config_saves_it() {
        let mut s = loaded_settings();
        edit_instance_to_porch(&mut s);
        let expected = {
            let mut c = config();
            c.instance = Some("porch".to_owned());
            c
        };

        let action = s.on_key(Key::Char('s'));

        match action {
            Some(Action::Do(Effect::SaveSettings(boxed))) => assert_eq!(*boxed, expected),
            other => panic!("expected a save effect, got {other:?}"),
        }
        assert_eq!(s.note, "Saving…");
    }

    #[test]
    fn s_with_a_zone_pointing_at_an_unknown_light_points_at_it_instead_of_saving() {
        let mut c = config();
        c.zones[1].light_name = LightId::new("ghost");
        let mut s = Settings::default();
        s.loaded(Ok(c));

        let action = s.on_key(Key::Char('s'));

        assert_eq!(action, None);
        assert!(s.problem);
        assert_eq!(s.selected_field(), Some(Field::Zone(1, ZoneField::Light)));
    }

    #[test]
    fn s_after_deleting_every_light_points_at_add_a_light() {
        let mut s = loaded_settings();
        place(&mut s, Field::Light(0, LightField::Name));
        s.on_key(Key::Char('d'));
        s.on_key(Key::Char('d')); // deletes lamp1; cursor lands on lamp2
        s.on_key(Key::Char('d'));
        s.on_key(Key::Char('d')); // deletes lamp2

        let action = s.on_key(Key::Char('s'));

        assert_eq!(action, None);
        assert!(s.problem);
        assert!(matches!(&s.rows()[s.cursor()], Row::Add { group: Group::Light, .. }));
    }

    #[test]
    fn saved_ok_with_restart_needed_clears_dirty_and_mentions_the_next_start() {
        let mut s = loaded_settings();
        edit_instance_to_porch(&mut s);
        assert!(s.dirty());

        s.saved(Ok(Saved { path: PathBuf::from("/cfg.yaml"), restart_needed: true }));

        assert!(!s.dirty());
        assert!(s.note.contains("next start"));
    }

    #[test]
    fn saved_err_sets_a_problem() {
        let mut s = loaded_settings();

        s.saved(Err(anyhow!("disk full")));

        assert!(s.problem);
        assert_eq!(s.note, "disk full");
    }

    // --- Load failure ------------------------------------------------------

    #[test]
    fn loaded_err_sets_load_error_and_clears_the_rows() {
        let mut s = Settings::default();

        s.loaded(Err(anyhow!("bad yaml")));

        assert!(s.load_error.is_some());
        assert!(s.rows().is_empty());
        assert!(s.problem);
    }

    #[test]
    fn after_a_load_error_only_o_and_q_do_anything() {
        let mut s = Settings::default();
        s.loaded(Err(anyhow!("bad yaml")));

        assert_eq!(s.on_key(Key::Down), None);
        assert_eq!(s.on_key(Key::Char('s')), None);
        assert_eq!(s.on_key(Key::Char('o')), Some(Action::Do(Effect::Edit)));
        assert_eq!(s.on_key(Key::Char('q')), Some(Action::Leave));
    }

    #[test]
    fn after_a_load_error_esc_also_leaves() {
        let mut s = Settings::default();
        s.loaded(Err(anyhow!("bad yaml")));

        assert_eq!(s.on_key(Key::Esc), Some(Action::Leave));
    }

    // --- Opening -----------------------------------------------------------

    #[test]
    fn opening_resets_everything_including_a_stale_note() {
        let mut s = loaded_settings();
        s.note = "something stale".to_owned();
        s.problem = true;

        s.opening();

        assert_eq!(s.note, "Loading the config…");
        assert!(!s.problem);
        assert!(s.rows().is_empty());
        assert!(s.load_error.is_none());
        assert!(!s.dirty());
    }

    // --- Pure helpers --------------------------------------------------

    #[test]
    fn next_choice_wraps_forward_and_backward() {
        assert_eq!(next_choice(Some(0), 4, 1), 1);
        assert_eq!(next_choice(Some(3), 4, 1), 0);
        assert_eq!(next_choice(Some(0), 4, -1), 3);
    }

    #[test]
    fn next_choice_with_no_current_value_joins_at_the_end_the_move_came_from() {
        assert_eq!(next_choice(None, 4, 1), 0);
        assert_eq!(next_choice(None, 4, -1), 3);
    }

    #[test]
    fn whole_distinguishes_empty_non_numeric_and_out_of_range_text() {
        assert_eq!(whole::<u16>(""), Err("This setting needs a whole number.".to_owned()));
        assert!(whole::<u16>("abc").unwrap_err().contains("not a whole number"));
        assert!(whole::<u16>("70000").unwrap_err().contains("outside the range"));
        assert_eq!(whole::<u16>("1884"), Ok(1884));
    }

    #[test]
    fn offset_finds_the_byte_boundary_of_a_multi_byte_character() {
        // "café": c, a, f are one byte each; é is two bytes.
        assert_eq!(offset("café", 3), 3);
        assert_eq!(offset("café", 4), 5); // past the end
    }

    #[test]
    fn backspace_removes_one_character_even_when_it_spans_multiple_bytes() {
        let mut s = loaded_settings();
        place(&mut s, Field::Mqtt(MqttField::Broker));
        s.on_key(Key::Enter);
        {
            let edit = s.edit.as_mut().unwrap();
            edit.buffer = "café".to_owned();
            edit.cursor = 4;
        }

        s.on_key(Key::Backspace);

        assert_eq!(s.editing().unwrap().buffer, "caf");
    }
}
