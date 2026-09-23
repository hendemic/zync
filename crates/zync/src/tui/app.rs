//! Everything the TUI knows, and every move a key press can make.
//!
//! Pure by construction. A key press changes state and may *ask* for work, but
//! nothing here opens a file, spawns a process or touches a terminal — the event
//! loop next door performs what comes back. That is what lets the whole screen
//! flow be driven from a test with no terminal attached, and it is also what
//! keeps the two blocking operations, start and stop, off the drawing thread.

use anyhow::{Error, Result};

use crate::ops::{EditOutcome, LogLevel, Started, Status, Stopped};

/// A key press, reduced to what the TUI acts on.
///
/// Its own type rather than crossterm's, so the state machine depends on nothing
/// that needs a terminal to exist. Translating one into the other is the event
/// loop's job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    /// Ctrl-C. Leaves from wherever it is pressed.
    Interrupt,
}

/// Work the event loop must do on the app's behalf.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Start,
    Stop,
    /// Open, or re-open, the log follower at this level.
    Follow(LogLevel),
    Edit,
    Quit,
}

/// Which screen has the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Home,
    Logs,
}

/// An operation running on a worker thread. Start blocks for the best part of a
/// second and stop for up to five, so both are shown as in progress and further
/// presses of either are ignored until the result lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    Starting,
    Stopping,
}

/// What the header reports, flattened out of `ops::Status`.
///
/// Flattened rather than held as the status itself so the state machine can be
/// built by hand: every field is a string or a number, and none of them needs a
/// config directory to exist.
pub struct Health {
    pub pid: Option<u32>,
    pub instance: String,
    pub config_path: String,
    pub log_path: String,
    /// Why the config cannot be used, when it cannot. Start will fail for as
    /// long as this is set, so the header says so rather than leaving it to be
    /// discovered by pressing Start.
    pub config_error: Option<String>,
}

impl Health {
    pub fn running(&self) -> bool {
        self.pid.is_some()
    }

    /// Stands in when even the paths could not be resolved, which leaves nothing
    /// true to report except the reason.
    pub fn unresolved(error: &Error) -> Self {
        Health {
            pid: None,
            instance: "unknown".to_owned(),
            config_path: "unavailable".to_owned(),
            log_path: "unavailable".to_owned(),
            config_error: Some(one_line(error)),
        }
    }
}

impl From<Status> for Health {
    fn from(status: Status) -> Self {
        Health {
            pid: status.pid,
            instance: status.instance,
            config_path: status.config_path.display().to_string(),
            log_path: status
                .log_path
                .map_or_else(|| "unavailable".to_owned(), |path| path.display().to_string()),
            config_error: status.config.err().as_ref().map(one_line),
        }
    }
}

/// The home screen's menu, in the order it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Start,
    Stop,
    Logs,
    Edit,
    Quit,
}

impl Item {
    pub const ALL: [Item; 5] = [Item::Start, Item::Stop, Item::Logs, Item::Edit, Item::Quit];

    /// The letter that reaches this item without walking the menu. Shown in the
    /// label, so the menu doubles as its own key legend.
    pub fn hotkey(self) -> char {
        match self {
            Item::Start => 's',
            Item::Stop => 'x',
            Item::Logs => 'l',
            Item::Edit => 'e',
            Item::Quit => 'q',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Item::Start => "Start",
            Item::Stop => "Stop",
            Item::Logs => "Logs",
            Item::Edit => "Edit settings",
            Item::Quit => "Quit",
        }
    }

    /// The half-line of explanation drawn beside the label.
    pub fn hint(self) -> &'static str {
        match self {
            Item::Start => "sync the lights in the background",
            Item::Stop => "stop syncing and fade the lights back",
            Item::Logs => "follow what this session is doing",
            Item::Edit => "open the config in your editor",
            Item::Quit => "leaves zync running",
        }
    }
}

/// The log view's own state: what has been read, and where in it we are looking.
pub struct Logs {
    /// Which threshold the follower was opened at. A read-time choice, since the
    /// file already holds the detail.
    pub level: LogLevel,
    pub lines: Vec<String>,
    /// Index of the topmost visible line.
    pub scroll: usize,
    /// Whether new output drags the view down with it. Scrolling up turns this
    /// off, and jumping to the bottom turns it back on.
    pub tail: bool,
    /// Why there is nothing, or nothing more, to read.
    pub problem: Option<String>,
    /// Rows the last frame had room for. Written by the view because the height
    /// is the one thing about scrolling that only the frame knows.
    pub height: usize,
}

impl Default for Logs {
    fn default() -> Self {
        Logs {
            level: LogLevel::Info,
            lines: Vec::new(),
            scroll: 0,
            tail: true,
            problem: None,
            height: 0,
        }
    }
}

impl Logs {
    /// Told by the view how many rows it got.
    ///
    /// Also where following and resizing are settled: a taller window shows more
    /// of the same tail, and a shorter one must not leave the view scrolled past
    /// the end of what there is.
    pub fn resize(&mut self, height: usize) {
        self.height = height;
        self.scroll = match self.tail {
            true => self.last_page(),
            false => self.scroll.min(self.last_page()),
        };
    }

    /// The lines on show, as the last `resize` sized the window.
    pub fn visible(&self) -> &[String] {
        let start = self.scroll.min(self.lines.len());
        let end = (start + self.height).min(self.lines.len());

        &self.lines[start..end]
    }

    /// Whether the view is sitting on the newest output rather than back in the
    /// history.
    pub fn at_bottom(&self) -> bool {
        self.scroll >= self.last_page()
    }

    /// The scroll position that puts the last line on the last row.
    fn last_page(&self) -> usize {
        self.lines.len().saturating_sub(self.height)
    }

    fn up(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
        // Looking back at something means new output must not yank the view away
        // from it.
        self.tail = false;
    }

    fn down(&mut self, rows: usize) {
        self.scroll = (self.scroll + rows).min(self.last_page());
        // Arriving at the bottom by scrolling is the same request as `G`.
        self.tail = self.at_bottom();
    }
}

/// The whole of the TUI's state.
pub struct App {
    pub screen: Screen,
    pub health: Health,
    /// Index into `Item::ALL`.
    pub selected: usize,
    /// The one-line result area at the foot of the home screen.
    pub message: String,
    pub busy: Option<Busy>,
    pub logs: Logs,
    /// Set once, and never unset: the event loop returns on seeing it.
    pub quit: bool,
}

impl App {
    pub fn new(health: Health) -> Self {
        App {
            screen: Screen::Home,
            health,
            selected: 0,
            message: String::new(),
            busy: None,
            logs: Logs::default(),
            quit: false,
        }
    }

    /// Whether activating an item would do anything. The view greys it when it
    /// would not.
    pub fn enabled(&self, item: Item) -> bool {
        match item {
            Item::Start => !self.health.running() && self.busy.is_none(),
            Item::Stop => self.health.running() && self.busy.is_none(),
            _ => true,
        }
    }

    pub fn on_key(&mut self, key: Key) -> Option<Effect> {
        // Ctrl-C answers from anywhere, including the log view, where `q` means
        // "back" rather than "leave".
        if key == Key::Interrupt {
            self.quit = true;
            return Some(Effect::Quit);
        }

        match self.screen {
            Screen::Home => self.home_key(key),
            Screen::Logs => self.logs_key(key),
        }
    }

    fn home_key(&mut self, key: Key) -> Option<Effect> {
        match key {
            // j and k are movement before they are letters, so they are matched
            // ahead of the hotkeys.
            Key::Up | Key::Char('k') => self.step(-1),
            Key::Down | Key::Char('j') => self.step(1),
            Key::Enter => return self.activate(Item::ALL[self.selected]),
            Key::Char(pressed) => {
                let (index, item) = Item::ALL
                    .into_iter()
                    .enumerate()
                    .find(|(_, item)| item.hotkey() == pressed)?;

                // The cursor follows the key, so what happened is visible in the
                // menu as well as in the message line.
                self.selected = index;

                return self.activate(item);
            }
            _ => {}
        }

        None
    }

    /// Wraps, so ↓ on the last item reaches the first.
    ///
    /// Disabled items are not skipped: a greyed Start is worth seeing as you
    /// pass it, and landing on it explains itself.
    fn step(&mut self, delta: isize) {
        let count = Item::ALL.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
    }

    fn activate(&mut self, item: Item) -> Option<Effect> {
        match item {
            // A second press while one is in flight would race the first, and
            // the message line already says what is happening.
            Item::Start | Item::Stop if self.busy.is_some() => None,
            Item::Start if self.health.running() => {
                self.message = match self.health.pid {
                    Some(pid) => format!("zync is already running (pid {pid})."),
                    None => "zync is already running.".to_owned(),
                };
                None
            }
            Item::Start => {
                self.busy = Some(Busy::Starting);
                self.message = "Starting…".to_owned();
                Some(Effect::Start)
            }
            Item::Stop if !self.health.running() => {
                self.message = "zync is not running, so there is nothing to stop.".to_owned();
                None
            }
            Item::Stop => {
                self.busy = Some(Busy::Stopping);
                self.message = "Stopping…".to_owned();
                Some(Effect::Stop)
            }
            Item::Logs => {
                self.screen = Screen::Logs;
                Some(Effect::Follow(self.logs.level))
            }
            Item::Edit => Some(Effect::Edit),
            Item::Quit => {
                self.quit = true;
                Some(Effect::Quit)
            }
        }
    }

    fn logs_key(&mut self, key: Key) -> Option<Effect> {
        // A page is whatever the window is showing, and never nothing, so a
        // frame that has not been drawn yet still moves.
        let page = self.logs.height.max(1);

        match key {
            Key::Esc | Key::Char('q') => self.screen = Screen::Home,
            Key::Up | Key::Char('k') => self.logs.up(1),
            Key::Down | Key::Char('j') => self.logs.down(1),
            Key::PageUp => self.logs.up(page),
            Key::PageDown => self.logs.down(page),
            Key::End | Key::Char('G') => self.logs.tail = true,
            Key::Home | Key::Char('g') => self.logs.up(usize::MAX),
            Key::Char('v') => {
                self.logs.level = match self.logs.level {
                    LogLevel::Debug => LogLevel::Info,
                    _ => LogLevel::Debug,
                };

                // The filtering happens as the file is read, so the level can
                // only change by reading it again.
                return Some(Effect::Follow(self.logs.level));
            }
            _ => {}
        }

        None
    }

    /// Takes the result of the start that was running on a worker thread.
    pub fn started(&mut self, outcome: Result<Started>) {
        self.busy = None;
        self.message = match outcome {
            Ok(started) => format!(
                "Started (pid {}, instance {}).",
                started.pid, started.instance
            ),
            // The config is the usual reason a start fails, and on a first run it
            // has only just been written — so say where to go next instead of
            // leaving `e` to be found.
            Err(e) if self.health.config_error.is_some() => {
                format!("{} Press e for Edit settings.", one_line(&e))
            }
            Err(e) => one_line(&e),
        };
    }

    /// Takes the result of the stop that was running on a worker thread.
    pub fn stopped(&mut self, outcome: Result<Stopped>) {
        self.busy = None;
        self.message = match outcome {
            Ok(stopped) if stopped.exited => {
                format!("Stopped zync ({}); the lights are fading back.", stopped.instance)
            }
            Ok(stopped) => format!(
                "Asked zync ({}) to stop but it is still running. Press l for the logs.",
                stopped.instance
            ),
            Err(e) => one_line(&e),
        };
    }

    /// Takes what became of an edit, once the editor has given the terminal back.
    pub fn edited(&mut self, outcome: Result<EditOutcome>) {
        self.message = match outcome {
            Ok(outcome) => match (&outcome.validation, outcome.restart_needed) {
                (Ok(_), true) => format!(
                    "The config at {} is valid; zync is running, so it takes effect on the next start.",
                    outcome.path.display()
                ),
                (Ok(_), false) => format!("The config at {} is valid.", outcome.path.display()),
                // The error's own context names the file, so the headline does
                // not repeat it.
                (Err(e), _) => format!("The config is not usable: {}", one_line(e)),
            },
            Err(e) => one_line(&e),
        };
    }

    /// Replaces everything on show, after the follower was opened or re-opened.
    pub fn logs_opened(&mut self, outcome: Result<Vec<String>>) {
        match outcome {
            Ok(lines) => {
                self.logs.problem = None;
                self.logs.lines = lines;
            }
            Err(e) => {
                self.logs.problem = Some(one_line(&e));
                self.logs.lines.clear();
            }
        }

        // A fresh view opens on the newest output, whatever the last one was
        // looking at.
        self.logs.tail = true;
        self.logs.scroll = 0;
    }

    /// Adds what the follower has read since the last tick.
    pub fn logs_appended(&mut self, lines: Vec<String>) {
        self.logs.lines.extend(lines);
    }

    /// Records why following stopped. Said once, rather than once per tick,
    /// because the event loop drops the reader that raised it.
    pub fn logs_failed(&mut self, error: &Error) {
        self.logs.problem = Some(one_line(error));
    }
}

/// An error's whole context chain, on one line.
///
/// `{e:#}` joins the chain with `: `, but an individual message may still carry
/// newlines of its own — the first-run one does — and these land in a message
/// area one line high. Broken lines are joined with a dash rather than a space,
/// since what follows one is a new sentence and would otherwise run into the
/// path in front of it.
fn one_line(error: &Error) -> String {
    format!("{error:#}")
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" — ")
}
