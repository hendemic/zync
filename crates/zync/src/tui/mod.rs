//! The interactive terminal interface.
//!
//! Setting the terminal up and taking it down again, and the loop in between.
//! What the keys mean lives in `app` and what it looks like in `view`; this is
//! the only part that performs anything, which is what keeps the blocking
//! operations — start, stop, the editor — from freezing the frame.

mod app;
mod view;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{self, Stdout};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::color;
use crate::ops::{self, LogFollower, LogLevel, LogView, Started, Stopped};
use app::{App, Effect, Health, Key, Screen};

/// How long the loop waits for a key before going round anyway. Matched to the
/// log follower's own suggested interval, so following costs no extra wake-ups.
const TICK: Duration = ops::FOLLOW_INTERVAL;

/// How often the header's answer to "is it running?" is renewed. A pid check and
/// a config read, so once a second costs nothing worth saving.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);

/// Opens the interface, and returns when the user leaves it.
pub fn run() -> Result<()> {
    let mut console = Console::enter()?;

    // The guard hands the shell back as this returns, so a failure inside the
    // loop leaves the terminal no worse than a clean exit does.
    event_loop(&mut console)
}

/// Owns raw mode and the alternate screen for as long as the interface is up.
///
/// A guard rather than a pair of calls because every way out — a `?`, a quit, a
/// panic — has to put the shell back, and nothing short of `Drop` plus a panic
/// hook covers all three.
struct Console {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Console {
    fn enter() -> Result<Self> {
        install_panic_hook();
        take_terminal()?;

        Ok(Console { terminal: fresh_terminal()? })
    }

    fn draw(&mut self, app: &mut App, painted: bool) -> Result<()> {
        self.terminal
            .draw(|frame| view::render(frame, app, painted))
            .context("Could not draw to the terminal")?;

        Ok(())
    }

    /// Hands the real terminal over for the duration of `work`.
    ///
    /// An editor draws its own screen and reads its own keys, so it needs raw
    /// mode off and our alternate screen out of the way.
    fn lend<T>(&mut self, work: impl FnOnce() -> T) -> Result<T> {
        give_terminal_back().context("Could not hand the terminal over")?;
        let outcome = work();
        take_terminal()?;

        // The screen we come back to is blank, and a terminal that has never
        // drawn is the honest description of that. Keeping the old one would
        // leave ratatui comparing against a frame nobody can see any more, and
        // asking it to clear instead costs a cursor-position round trip that a
        // slow link can lose.
        self.terminal = fresh_terminal()?;

        Ok(outcome)
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        // There is nothing useful to do about a failure here. The process is on
        // its way out either way, and complaining would mean printing to a
        // terminal that may still be in raw mode.
        let _ = give_terminal_back();
    }
}

fn fresh_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    Terminal::new(CrosstermBackend::new(io::stdout())).context("Could not take over the terminal")
}

/// Clearing is belt and braces: `1049` blanks the alternate screen on most
/// terminals, but not on all of them, and a first frame drawn over somebody
/// else's output is hard to read.
fn take_terminal() -> Result<()> {
    terminal::enable_raw_mode().context("Could not put the terminal into raw mode")?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Hide
    )
    .context("Could not open a full-screen view")?;

    Ok(())
}

/// The exact reverse, and harmless to call when it was never taken.
fn give_terminal_back() -> io::Result<()> {
    execute!(io::stdout(), LeaveAlternateScreen, Show)?;
    terminal::disable_raw_mode()
}

/// Puts the shell back before a panic is printed.
///
/// Without this a bug loses both the backtrace, which scrolls away with the
/// alternate screen, and the shell, which is left in raw mode.
fn install_panic_hook() {
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let _ = give_terminal_back();
        previous(info);
    }));
}

/// What a worker thread has finished doing.
///
/// Start blocks for the best part of a second and stop for up to five, which is
/// far too long to stop drawing for, so both answer over a channel instead.
enum Done {
    Started(Result<Started>),
    Stopped(Result<Stopped>),
}

fn event_loop(console: &mut Console) -> Result<()> {
    // The interface owns a terminal by construction, so the only question left
    // about colour is whether the viewer has opted out of it.
    let painted = !color::opted_out();

    let mut app = App::new(health());
    let (finished, results) = mpsc::channel();
    let mut follower: Option<LogFollower> = None;
    let mut renewed = Instant::now();

    loop {
        console.draw(&mut app, painted)?;

        // A resize, a mouse report, a key release: everything that is not a key
        // press means no more than "go round and draw again", which the loop
        // does anyway.
        if event::poll(TICK).context("Could not read the terminal")?
            && let Event::Key(key) = event::read().context("Could not read the terminal")?
            && key.kind == KeyEventKind::Press
            && let Some(pressed) = translate(key)
            && let Some(effect) = app.on_key(pressed)
        {
            perform(effect, &mut app, console, &finished, &mut follower)?;
        }

        for done in results.try_iter() {
            match done {
                Done::Started(outcome) => app.started(outcome),
                Done::Stopped(outcome) => app.stopped(outcome),
            }

            // Starting and stopping are exactly what changes the answer, so the
            // header does not wait out the rest of the interval to say so.
            app.health = health();
            renewed = Instant::now();
        }

        if let Some(reader) = follower.as_mut() {
            match reader.poll() {
                Ok(lines) => app.logs_appended(lines),
                // A reader that cannot read any more is dropped, so its reason is
                // reported once rather than once per tick.
                Err(e) => {
                    app.logs_failed(&e);
                    follower = None;
                }
            }
        }

        // Following lasts no longer than the screen that asked for it.
        if app.screen != Screen::Logs {
            follower = None;
        }

        if renewed.elapsed() >= STATUS_INTERVAL {
            app.health = health();
            renewed = Instant::now();
        }

        if app.quit {
            return Ok(());
        }
    }
}

fn perform(
    effect: Effect,
    app: &mut App,
    console: &mut Console,
    finished: &Sender<Done>,
    follower: &mut Option<LogFollower>,
) -> Result<()> {
    match effect {
        Effect::Start => off_thread(finished, Done::Started, ops::start),
        Effect::Stop => off_thread(finished, Done::Stopped, ops::stop),
        Effect::Follow(level) => *follower = open_logs(app, level),
        Effect::Edit => edit(app, console)?,
        // Leaving is the loop's own business; it looks on the way round.
        Effect::Quit => {}
    }

    Ok(())
}

/// Runs a blocking operation somewhere the event loop is not, so the frame keeps
/// being redrawn while it waits.
fn off_thread<T: Send + 'static>(
    finished: &Sender<Done>,
    wrap: fn(Result<T>) -> Done,
    work: fn() -> Result<T>,
) {
    let finished = finished.clone();

    thread::spawn(move || {
        // A send that fails means the loop has already gone, and there is nobody
        // left to tell.
        let _ = finished.send(wrap(work()));
    });
}

/// Opens a follower on this session's log and hands the history to the app.
///
/// A log file that is not there yet is something to show rather than something
/// to unwind on: on a first run there is genuinely nothing to read.
fn open_logs(app: &mut App, level: LogLevel) -> Option<LogFollower> {
    // Lines are ignored for a session view, which is shown whole.
    let view = LogView { level, session: true, lines: 0, follow: false };

    match LogFollower::open(view) {
        Ok((follower, history)) => {
            app.logs_opened(Ok(history));
            Some(follower)
        }
        Err(e) => {
            app.logs_opened(Err(e));
            None
        }
    }
}

/// Runs the editor on the real terminal, then takes it back.
fn edit(app: &mut App, console: &mut Console) -> Result<()> {
    let outcome = console.lend(ops::edit_config)?;

    app.edited(outcome);
    // The instance name is read out of the config, so it may have just changed.
    app.health = health();

    Ok(())
}

/// What the header reports, with the reason in its place when even the paths
/// could not be resolved.
fn health() -> Health {
    match ops::status() {
        Ok(status) => Health::from(status),
        Err(e) => Health::unresolved(&e),
    }
}

/// Crossterm's key events, narrowed to the ones the interface acts on.
///
/// Narrowed here rather than in the state machine, which is what lets the state
/// machine know nothing about crossterm and be driven without a terminal.
fn translate(event: KeyEvent) -> Option<Key> {
    // Ctrl-C is the one chord, and no other modified key means anything here —
    // least of all as the plain letter it would otherwise become.
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        return matches!(event.code, KeyCode::Char('c')).then_some(Key::Interrupt);
    }

    let key = match event.code {
        KeyCode::Char(pressed) => Key::Char(pressed),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        _ => return None,
    };

    Some(key)
}
