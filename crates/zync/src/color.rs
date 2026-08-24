//! Minimal ANSI colour, shared by command output and `zync logs`.
//!
//! Every function takes an explicit `enabled` bool rather than checking the
//! terminal itself: a caller printing many lines checks once per run, and it
//! keeps this module pure and easy to test without a real terminal attached.

use std::io::IsTerminal;

/// Whether stdout is an actual terminal and the viewer has not opted out via
/// https://no-color.org. Piping to `grep` or a file must see plain text.
pub fn enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
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

/// SGR code for a level token, keyed by exactly the words the formatter
/// writes (`TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`). A token that does not
/// match one of these exactly is left uncoloured rather than guessed at.
pub fn level_code(token: &str) -> Option<&'static str> {
    match token {
        "TRACE" => Some("2"),
        "DEBUG" => Some("36"),
        "INFO" => Some("32"),
        "WARN" => Some("33"),
        "ERROR" => Some("31;1"),
        _ => None,
    }
}
