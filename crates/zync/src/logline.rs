//! The shape of a log line, read without deciding how it should look.
//!
//! Both front ends emphasise the same three pieces of a line — timestamp, level,
//! everything else — and both have to agree on where those pieces start and end.
//! Finding them lives here, once; the escape codes or terminal styles stay with
//! whichever front end is drawing.

/// A level, as the tracing formatter writes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Severity {
    /// The severity a token names, or `None` when it is not one of the five
    /// words the formatter writes.
    ///
    /// Matched exactly. A token that merely looks level-ish is not a level,
    /// rather than a level we failed to recognise.
    pub fn of(token: &str) -> Option<Self> {
        match token {
            "TRACE" => Some(Self::Trace),
            "DEBUG" => Some(Self::Debug),
            "INFO" => Some(Self::Info),
            "WARN" => Some(Self::Warn),
            "ERROR" => Some(Self::Error),
            _ => None,
        }
    }

    /// The word the formatter writes for this level.
    pub fn token(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// One log line, split into the pieces a reader wants to treat differently.
///
/// Every piece borrows from the line it was read out of, and they concatenate
/// back into it exactly — timestamp, gap, level token, rest — so emphasising
/// part of a line can never quietly drop the rest of it.
pub struct LogLine<'a> {
    /// The line's first token, when it is a timestamp.
    pub timestamp: Option<&'a str>,
    /// Whatever sits between the timestamp and the level. The formatter
    /// right-aligns levels to five columns, so `INFO` and `WARN` arrive with an
    /// extra leading space that has to survive being coloured.
    pub gap: &'a str,
    pub level: Option<Severity>,
    /// Target and message: everything a reader passes through untouched.
    pub rest: &'a str,
}

/// Reads `<timestamp> <LEVEL> <target>: <message>` off a line.
///
/// Both pieces are found positionally rather than reassembled from tokens: the
/// timestamp, if present, is the line's first token, and the level is the token
/// after it. A line of any other shape — a panic, a wrapped line — comes back as
/// `rest` alone, which is the same thing as saying "print it as it stands".
pub fn parse(line: &str) -> LogLine<'_> {
    let (timestamp, tail) = match line.find(char::is_whitespace) {
        Some(end) if looks_like_timestamp(&line[..end]) => (Some(&line[..end]), &line[end..]),
        _ => (None, line),
    };

    let found = tail
        .split_whitespace()
        .next()
        .and_then(|token| Severity::of(token).map(|severity| (token, severity)))
        .and_then(|(token, severity)| tail.find(token).map(|at| (at, token, severity)));

    match found {
        Some((at, token, severity)) => LogLine {
            timestamp,
            gap: &tail[..at],
            level: Some(severity),
            rest: &tail[at + token.len()..],
        },
        None => LogLine { timestamp, gap: "", level: None, rest: tail },
    }
}

/// The formatter's timestamp shape: RFC 3339 with a literal `Z`, e.g.
/// `2026-08-24T06:42:01.006965Z`. Long enough, and specific enough, that the
/// first word of a panic or another unrelated line will never satisfy it.
pub fn looks_like_timestamp(token: &str) -> bool {
    token.len() >= 20 && token.contains('T') && token.ends_with('Z')
}
