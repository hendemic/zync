//! Minimal ANSI colour, shared by command output and `zync logs`.
//!
//! Every function takes an explicit `enabled` bool rather than checking the
//! terminal itself: a caller printing many lines checks once per run, and it
//! keeps this module pure and easy to test without a real terminal attached.

use std::io::IsTerminal;

use crate::logline::Severity;

/// Whether stdout is an actual terminal and the viewer has not opted out via
/// https://no-color.org. Piping to `grep` or a file must see plain text.
pub fn enabled() -> bool {
    std::io::stdout().is_terminal() && !opted_out()
}

/// Whether the viewer has opted out of colour via https://no-color.org.
///
/// Separate from `enabled` for the sake of a caller that already knows it owns a
/// terminal. The TUI has one by construction, so the opt-out is the only
/// question left for it to ask.
pub fn opted_out() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

fn paint(code: &str, text: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn green(text: &str, enabled: bool) -> String {
    paint("32", text, enabled)
}

pub fn yellow(text: &str, enabled: bool) -> String {
    paint("33", text, enabled)
}

/// De-emphasised detail: a label, a hint, a log line's timestamp.
pub fn dim(text: &str, enabled: bool) -> String {
    paint("2", text, enabled)
}

/// Dim and italic, for a log line's timestamp specifically.
pub fn dim_italic(text: &str, enabled: bool) -> String {
    paint("2;3", text, enabled)
}

/// A log line's level token, painted by severity.
pub fn level(severity: Severity, enabled: bool) -> String {
    paint(level_code(severity), severity.token(), enabled)
}

/// SGR code for a level. Which token carries which severity is `logline`'s
/// business; this is only the palette, so the two cannot drift apart.
pub fn level_code(severity: Severity) -> &'static str {
    match severity {
        Severity::Trace => "2",
        Severity::Debug => "36",
        Severity::Info => "32",
        Severity::Warn => "33",
        Severity::Error => "31;1",
    }
}
