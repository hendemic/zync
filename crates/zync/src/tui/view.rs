//! Drawing.
//!
//! Reads the app's state and writes nothing back to it, with one exception: the
//! log view tells `Logs` how many rows it got, because the height of a frame is
//! the one thing about scrolling that only the frame knows.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Wrap};

use crate::logline::{self, Severity};
use crate::ops::LogLevel;
use crate::tui::app::{App, Health, Item, Logs, Screen};

/// Width of the header's label column, so the values line up under each other.
const LABEL_COLUMN: usize = 9;

/// Width of the menu's label column, so the hints line up.
const HINT_COLUMN: usize = 15;

/// Rows the message area gets, borders included. Three lines of wrapped text is
/// enough for an `anyhow` chain naming a file and what was wrong with it.
const MESSAGE_HEIGHT: u16 = 5;

/// The log view's key legend. Drawn where a problem would otherwise go, since
/// there is only ever one of the two worth reading.
const LOG_KEYS: &str = " ↑/↓ scroll · PgUp/PgDn page · G bottom · v info/debug · q back";

pub fn render(frame: &mut Frame, app: &mut App, painted: bool) {
    let palette = Palette::new(painted);

    match app.screen {
        Screen::Home => home(frame, app, &palette),
        Screen::Logs => logs(frame, &mut app.logs, &palette),
    }
}

fn home(frame: &mut Frame, app: &App, palette: &Palette) {
    let facts = header(&app.health, palette);
    let rows = menu(app, palette);

    // Both panels are exactly as tall as what is in them, and the message area
    // is pinned to the foot of the screen with the slack between them.
    let areas = Layout::vertical([
        Constraint::Length(facts.len() as u16 + 2),
        Constraint::Length(rows.len() as u16 + 2),
        Constraint::Min(0),
        Constraint::Length(MESSAGE_HEIGHT),
    ])
    .split(frame.area());

    frame.render_widget(
        Paragraph::new(facts).block(panel(" zync ", palette)),
        areas[0],
    );
    frame.render_widget(
        Paragraph::new(rows).block(panel(" what now ", palette)),
        areas[1],
    );
    frame.render_widget(
        Paragraph::new(app.message.as_str())
            .wrap(Wrap { trim: true })
            .block(panel("", palette)),
        areas[3],
    );
}

/// Whether zync is running, and where everything it uses lives.
fn header<'a>(health: &'a Health, palette: &Palette) -> Vec<Line<'a>> {
    let state = match health.pid {
        Some(pid) => Span::styled(format!("running (pid {pid})"), palette.running()),
        None => Span::styled("not running", palette.stopped()),
    };

    let mut lines = vec![
        Line::from(vec![label("status", palette), state]),
        field("instance", &health.instance, palette),
        field("config", &health.config_path, palette),
        field("logs", &health.log_path, palette),
    ];

    // Start will fail for as long as this is set, so it belongs beside the rest
    // of the facts rather than waiting to be discovered by pressing Start.
    if let Some(problem) = &health.config_error {
        lines.push(Line::from(vec![
            Span::styled(pad("problem", LABEL_COLUMN), palette.problem()),
            Span::raw(problem.as_str()),
        ]));
    }

    lines
}

fn field<'a>(name: &str, value: &'a str, palette: &Palette) -> Line<'a> {
    Line::from(vec![label(name, palette), Span::raw(value)])
}

fn label(name: &str, palette: &Palette) -> Span<'static> {
    Span::styled(pad(name, LABEL_COLUMN), palette.dim())
}

fn menu(app: &App, palette: &Palette) -> Vec<Line<'static>> {
    Item::ALL
        .into_iter()
        .enumerate()
        .map(|(index, item)| row(item, app.enabled(item), index == app.selected, palette))
        .collect()
}

/// One menu item: a cursor, its letter, its label, and what it does.
///
/// The row's own style carries selection and being disabled, and the spans patch
/// their emphasis on top of it — so a greyed row stays grey throughout, letter
/// included.
fn row(item: Item, enabled: bool, selected: bool, palette: &Palette) -> Line<'static> {
    // A disabled item is still worth landing on, so the cursor marks it either
    // way; what changes is the colour of everything after it.
    let cursor = match selected {
        true => " > ",
        false => "   ",
    };

    let (key, hint) = match enabled {
        true => (
            Span::styled(item.hotkey().to_string(), palette.key()),
            Span::styled(item.hint(), palette.dim()),
        ),
        false => (
            Span::raw(item.hotkey().to_string()),
            Span::raw(item.hint()),
        ),
    };

    let base = match (enabled, selected) {
        (true, true) => palette.selected(),
        (true, false) => Style::new(),
        (false, true) => palette.disabled().patch(palette.selected()),
        (false, false) => palette.disabled(),
    };

    Line::from(vec![
        Span::raw(cursor),
        key,
        Span::raw(format!("  {}", pad(item.label(), HINT_COLUMN))),
        hint,
    ])
    .style(base)
}

fn logs(frame: &mut Frame, logs: &mut Logs, palette: &Palette) {
    let areas = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(frame.area());

    let title = format!(
        " logs · this session · {} · {} ",
        level_name(logs.level),
        match logs.tail {
            true => "following",
            false => "paused",
        }
    );
    let block = panel(&title, palette);

    // Everything about scrolling depends on how much room the border left.
    logs.resize(block.inner(areas[0]).height as usize);

    let body: Vec<Line> = match logs.lines.is_empty() {
        true => vec![Line::styled(nothing_to_show(logs), palette.dim())],
        false => logs
            .visible()
            .iter()
            .map(|line| log_line(line, palette))
            .collect(),
    };

    // Not wrapped: one log line to one row is what keeps scrolling honest, so a
    // long line is cut at the right edge instead of pushing the rest down.
    frame.render_widget(Paragraph::new(body).block(block), areas[0]);
    frame.render_widget(Paragraph::new(footer(logs, palette)), areas[1]);
}

/// What to put in an empty log view: the reason if there is one, and otherwise
/// the plain truth.
fn nothing_to_show(logs: &Logs) -> String {
    logs.problem
        .clone()
        .unwrap_or_else(|| "Nothing logged in this session yet.".to_owned())
}

/// The key legend, or a problem in its place when the body has no room to
/// report one.
fn footer(logs: &Logs, palette: &Palette) -> Line<'static> {
    match (&logs.problem, logs.lines.is_empty()) {
        (Some(problem), false) => Line::styled(format!(" {problem}"), palette.problem()),
        _ => Line::styled(LOG_KEYS, palette.dim()),
    }
}

/// One log line, with the timestamp pushed back and the level coloured by
/// severity. Where those pieces are is `logline`'s business.
fn log_line<'a>(line: &'a str, palette: &Palette) -> Line<'a> {
    let parsed = logline::parse(line);
    let mut spans = Vec::with_capacity(4);

    if let Some(timestamp) = parsed.timestamp {
        spans.push(Span::styled(timestamp, palette.timestamp()));
    }
    spans.push(Span::raw(parsed.gap));
    if let Some(severity) = parsed.level {
        spans.push(Span::styled(severity.token(), palette.level(severity)));
    }
    spans.push(Span::raw(parsed.rest));

    Line::from(spans)
}

/// The word for the threshold the log view is reading at. Only info and debug
/// are reachable from here, but naming them all keeps the view honest if that
/// changes.
fn level_name(level: LogLevel) -> &'static str {
    match level {
        LogLevel::All => "all",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

/// The frame every panel sits in.
fn panel<'a>(title: &'a str, palette: &Palette) -> Block<'a> {
    let block = Block::bordered()
        .border_style(palette.frame())
        .padding(Padding::horizontal(1));

    match title.is_empty() {
        true => block,
        false => block.title(Span::styled(title, palette.dim())),
    }
}

fn pad(text: &str, width: usize) -> String {
    format!("{text:<width$}")
}

/// The styles the TUI draws with.
///
/// Resolved once, so honouring `NO_COLOR` is this type's problem rather than
/// something every call site has to remember.
struct Palette {
    painted: bool,
}

impl Palette {
    fn new(painted: bool) -> Self {
        Palette { painted }
    }

    /// Drops colour when the viewer has opted out of it. Modifiers — bold, dim,
    /// reversed, italic — are not colour, so they stay: without them the menu
    /// cursor and the de-emphasis would vanish along with the palette.
    fn of(&self, style: Style) -> Style {
        match self.painted {
            true => style,
            false => Style::new().add_modifier(style.add_modifier),
        }
    }

    fn running(&self) -> Style {
        self.of(Style::new().fg(Color::Green).add_modifier(Modifier::BOLD))
    }

    fn stopped(&self) -> Style {
        self.of(Style::new().fg(Color::Yellow))
    }

    fn problem(&self) -> Style {
        self.of(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    }

    fn dim(&self) -> Style {
        self.of(Style::new().add_modifier(Modifier::DIM))
    }

    fn key(&self) -> Style {
        self.of(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    }

    fn disabled(&self) -> Style {
        self.of(Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM))
    }

    fn selected(&self) -> Style {
        self.of(Style::new().add_modifier(Modifier::REVERSED))
    }

    fn frame(&self) -> Style {
        self.of(Style::new().fg(Color::DarkGray))
    }

    fn timestamp(&self) -> Style {
        self.of(Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC))
    }

    /// The same severity palette the plain-terminal output uses, as styles
    /// rather than escape codes.
    fn level(&self, severity: Severity) -> Style {
        self.of(match severity {
            Severity::Trace => Style::new().add_modifier(Modifier::DIM),
            Severity::Debug => Style::new().fg(Color::Cyan),
            Severity::Info => Style::new().fg(Color::Green),
            Severity::Warn => Style::new().fg(Color::Yellow),
            Severity::Error => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        })
    }
}
