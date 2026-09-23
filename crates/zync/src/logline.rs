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

#[cfg(test)]
mod tests {
    use super::*;

    /// Puts a parsed line back together, to check the pieces really do
    /// concatenate into what was read.
    fn reassemble(parsed: &LogLine) -> String {
        format!(
            "{}{}{}{}",
            parsed.timestamp.unwrap_or(""),
            parsed.gap,
            parsed.level.map(Severity::token).unwrap_or(""),
            parsed.rest
        )
    }

    #[test]
    fn a_standard_info_line_parses_into_its_pieces_and_reassembles_exactly() {
        let line = "2026-08-24T06:00:00.0Z  INFO zync::cli: hello";
        let parsed = parse(line);

        assert_eq!(parsed.timestamp, Some("2026-08-24T06:00:00.0Z"));
        assert_eq!(parsed.gap, "  ");
        assert_eq!(parsed.level, Some(Severity::Info));
        assert_eq!(parsed.rest, " zync::cli: hello");
        assert_eq!(reassemble(&parsed), line);
    }

    /// INFO and WARN are both four letters, so the formatter's five-column
    /// right alignment pads them the same way.
    #[test]
    fn a_warn_line_carries_the_same_padding_as_info_and_reassembles_exactly() {
        let line = "2026-08-24T06:00:02.0Z  WARN zync_adapters::mqtt: something odd";
        let parsed = parse(line);

        assert_eq!(parsed.gap, "  ");
        assert_eq!(parsed.level, Some(Severity::Warn));
        assert_eq!(reassemble(&parsed), line);
    }

    /// ERROR already fills all five columns, so there is no padding beyond the
    /// single space that separates it from the timestamp.
    #[test]
    fn an_error_line_has_no_extra_padding_and_reassembles_exactly() {
        let line = "2026-08-24T06:00:03.0Z ERROR zync_adapters::mqtt: broker unreachable";
        let parsed = parse(line);

        assert_eq!(parsed.gap, " ");
        assert_eq!(parsed.level, Some(Severity::Error));
        assert_eq!(reassemble(&parsed), line);
    }

    #[test]
    fn a_line_with_a_level_but_no_timestamp_parses_with_no_timestamp() {
        let parsed = parse("ERROR something");

        assert_eq!(parsed.timestamp, None);
        assert_eq!(parsed.gap, "");
        assert_eq!(parsed.level, Some(Severity::Error));
        assert_eq!(parsed.rest, " something");
    }

    #[test]
    fn a_panic_line_has_no_level_and_reassembles_from_rest_alone() {
        let line = "thread 'main' panicked at src/lib.rs:1:1";
        let parsed = parse(line);

        assert_eq!(parsed.timestamp, None);
        assert_eq!(parsed.level, None);
        assert_eq!(parsed.rest, line);
        assert_eq!(reassemble(&parsed), line);
    }

    #[test]
    fn an_empty_line_parses_to_nothing() {
        let parsed = parse("");

        assert_eq!(parsed.timestamp, None);
        assert_eq!(parsed.gap, "");
        assert_eq!(parsed.level, None);
        assert_eq!(parsed.rest, "");
    }

    #[test]
    fn severity_of_round_trips_with_token_for_every_level() {
        for severity in [
            Severity::Trace,
            Severity::Debug,
            Severity::Info,
            Severity::Warn,
            Severity::Error,
        ] {
            assert_eq!(Severity::of(severity.token()), Some(severity));
        }
    }

    /// A token that merely looks level-ish is not a level, rather than a level
    /// we failed to recognise — so only the exact five words match.
    #[test]
    fn severity_of_rejects_anything_that_is_not_an_exact_uppercase_token() {
        assert_eq!(Severity::of("info"), None);
        assert_eq!(Severity::of("INFORMATION"), None);
        assert_eq!(Severity::of(""), None);
    }
}
