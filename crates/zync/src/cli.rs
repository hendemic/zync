//! Command line surface.
//!
//! Presentation only: each subcommand translates arguments into one call on
//! `ops` and turns what comes back into terminal output. Nothing here decides
//! what an operation means — that lives in `ops`, so a second front end can
//! reach the same behaviour without going through this file.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::thread;
use zync_core::domain::Config;

use crate::color;
use crate::logline;
use crate::ops;

/// How many lines `zync logs` shows when neither --session nor --lines says
/// otherwise.
const DEFAULT_LOG_LINES: usize = 40;

#[derive(Parser)]
#[command(
    name = "zync",
    version,
    about = "Drive Zigbee lights from what is on your screen"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start syncing the lights, in the background.
    Start {
        /// Run in this terminal instead of detaching, logging as it goes.
        #[arg(long, short)]
        foreground: bool,
    },
    /// Stop syncing and hand the lights back.
    Stop,
    /// Report whether zync is running.
    Status,
    /// Show what zync has been doing.
    Logs {
        /// Keep printing new output until interrupted. Implies --session
        /// unless --lines is given explicitly: watching what happens next
        /// should not open with a page of whatever a past run left behind.
        #[arg(long, short)]
        follow: bool,
        /// Show only the most recent run, in full.
        #[arg(long, short)]
        session: bool,
        /// Include the per-frame detail the log already holds.
        #[arg(long, short, conflicts_with = "level")]
        verbose: bool,
        /// Lowest level to show. Defaults to events only.
        #[arg(long, value_enum, default_value_t = ops::LogLevel::Info)]
        level: ops::LogLevel,
        /// How many existing lines to show first. Ignored with --session,
        /// and with --follow unless given explicitly.
        #[arg(long, short = 'n')]
        lines: Option<usize>,
    },
    /// Edit the configuration in your editor, and check what you saved.
    Config {
        /// Check the configuration without opening an editor. Exits non-zero if
        /// it is not usable.
        #[arg(long, conflicts_with = "path")]
        check: bool,
        /// Print the path to the configuration file and nothing else.
        #[arg(long)]
        path: bool,
    },
    /// The detached service process. Not for direct use.
    #[command(name = "__daemon", hide = true)]
    Daemon,
}

/// Which log destinations a command wants.
///
/// The parent of a detached service must not write to the same file as the
/// service itself, so only one process at a time logs to disk.
pub enum Logging {
    /// The service: the log file only, since it has no terminal.
    Service,
    /// A foreground run: the log file and the terminal.
    Foreground,
    /// A short-lived command: the terminal only.
    Client,
    /// Output belongs to the command itself; logs would only get in the way.
    Silent,
}

/// Following without being told otherwise means "just this session, as it
/// happens" — mixing in whatever a past run logged reads as noise, not
/// history. An explicit --lines opts back into the old tail-then-follow
/// behaviour even while following.
fn scope_to_session(follow: bool, session: bool, lines: Option<usize>) -> bool {
    session || (follow && lines.is_none())
}

impl Command {
    pub fn logging(&self) -> Logging {
        match self {
            Command::Daemon => Logging::Service,
            Command::Start { foreground: true } => Logging::Foreground,
            // `logs` is output. So is `config`: the editor takes over the
            // terminal it would log to, and `--path` is there to be piped into
            // something else, which a stray warning line would corrupt.
            Command::Logs { .. } | Command::Config { .. } => Logging::Silent,
            _ => Logging::Client,
        }
    }

    pub fn run(self) -> Result<()> {
        match self {
            Command::Start { foreground: false } => start(),
            // The service takes the same path as a foreground run; only the
            // logging set up above differs.
            Command::Start { foreground: true } | Command::Daemon => ops::run_foreground(),
            Command::Stop => stop(),
            Command::Status => status(),
            Command::Logs { follow, session, verbose, level, lines } => {
                tail(ops::LogView {
                    level: if verbose { ops::LogLevel::Debug } else { level },
                    session: scope_to_session(follow, session, lines),
                    lines: lines.unwrap_or(DEFAULT_LOG_LINES),
                    follow,
                })
            }
            Command::Config { check, path } => config(check, path),
        }
    }
}

fn start() -> Result<()> {
    let started = ops::start()?;
    let painted = color::enabled();

    println!(
        "zync is {} in the background (pid {}, instance {}).",
        color::green("running", painted),
        started.pid,
        started.instance
    );
    println!("  {}", color::dim("zync logs -f    follow what it is doing", painted));
    println!("  {}", color::dim("zync logs -sv   everything from this run, in detail", painted));
    println!("  {}", color::dim("zync stop       stop it and fade the lights back", painted));

    Ok(())
}

fn stop() -> Result<()> {
    let stopped = ops::stop()?;
    let instance = stopped.instance;
    let painted = color::enabled();

    if stopped.exited {
        println!("{} zync ({instance}). The lights are fading back.", color::green("Stopped", painted));
    } else {
        println!(
            "{} zync ({instance}) to stop, but it is still running. See `zync logs`.",
            color::yellow("Asked", painted)
        );
    }

    Ok(())
}

fn status() -> Result<()> {
    let status = ops::status()?;
    let painted = color::enabled();

    match status.pid {
        Some(pid) => println!("zync is {} (pid {pid}).", color::green("running", painted)),
        None => println!("zync is {}.", color::yellow("not running", painted)),
    }

    let log = status
        .log_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "unavailable".to_string());

    println!("  {}  {}", color::dim("instance", painted), status.instance);
    println!("  {}    {}", color::dim("config", painted), status.config_path.display());
    println!("  {}      {log}", color::dim("logs", painted));

    Ok(())
}

/// Prints the tail of the log file, optionally following it.
///
/// The reader hands back lines and says nothing about when to ask for more, so
/// the sleeping happens here, where interrupting it is the terminal's business.
fn tail(view: ops::LogView) -> Result<()> {
    let follow = view.follow;
    let (mut follower, history) = ops::LogFollower::open(view)?;

    let painted = color::enabled();
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    for line in &history {
        write_line(&mut out, line, painted)?;
    }
    out.flush()?;

    if !follow {
        return Ok(());
    }

    loop {
        thread::sleep(ops::FOLLOW_INTERVAL);

        for line in follower.poll()? {
            write_line(&mut out, &line, painted)?;
        }
        out.flush()?;
    }
}

fn config(check: bool, path_only: bool) -> Result<()> {
    if path_only {
        println!("{}", ops::config_path()?.display());
        return Ok(());
    }

    if check {
        // Reported as an error rather than as output, so a script gets the
        // non-zero exit and the message lands on stderr with every other
        // failure.
        let config = ops::check_config()?;
        report_config(&ops::config_path()?, &Ok(config), false);

        return Ok(());
    }

    let outcome = ops::edit_config()?;
    report_config(&outcome.path, &outcome.validation, outcome.restart_needed);

    Ok(())
}

/// Says whether what is on disk is usable.
///
/// A bad config is printed rather than returned as an error: after an edit it is
/// something to go and fix, not a failed command, and the error's own context
/// already names the file and what was wrong with it.
fn report_config(path: &Path, validation: &Result<Config>, restart_needed: bool) {
    let painted = color::enabled();

    match validation {
        Ok(_) => {
            println!("The config at {} is {}.", path.display(), color::green("valid", painted));

            if restart_needed {
                println!(
                    "  {}",
                    color::dim("zync is running; it takes effect on the next `zync start`", painted)
                );
            }
        }
        Err(e) => {
            // The error's own context names the file, so the headline does not
            // repeat it.
            println!("The config is {}:", color::yellow("not usable", painted));
            println!("  {e:#}");
            println!("  {}", color::dim("run `zync config` again to fix it", painted));
        }
    }
}

/// Writes one line: the timestamp dimmed and italicised, the level token
/// coloured by severity, everything else — target, message — passed through
/// untouched.
///
/// Where those pieces are is `logline`'s business, so a line of some other
/// shape reassembles into itself and prints as it stands.
fn write_line(out: &mut impl Write, line: &str, color: bool) -> Result<()> {
    if !color {
        writeln!(out, "{line}")?;
        return Ok(());
    }

    let parsed = logline::parse(line);
    let timestamp = parsed
        .timestamp
        .map_or_else(String::new, |token| color::dim_italic(token, true));
    let level = parsed
        .level
        .map_or_else(String::new, |severity| color::level(severity, true));

    writeln!(out, "{timestamp}{}{level}{}", parsed.gap, parsed.rest)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap needs the name as a literal, so it cannot share the constant that
    /// `spawn_detached` passes. This is the guard against them drifting apart.
    #[test]
    fn the_hidden_daemon_command_matches_what_is_spawned() {
        assert!(
            Cli::command()
                .get_subcommands()
                .any(|sub| sub.get_name() == crate::service::DAEMON_COMMAND),
            "no subcommand named {}",
            crate::service::DAEMON_COMMAND
        );
    }

    #[test]
    fn start_detaches_unless_foreground_is_asked_for() {
        let background = Cli::try_parse_from(["zync", "start"]).unwrap();
        let foreground = Cli::try_parse_from(["zync", "start", "--foreground"]).unwrap();

        assert!(matches!(background.command, Command::Start { foreground: false }));
        assert!(matches!(foreground.command, Command::Start { foreground: true }));
    }

    /// Checking and printing the path are both "don't open an editor", but they
    /// answer different questions and combining them would print two things.
    #[test]
    fn checking_and_printing_the_config_path_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["zync", "config", "--check", "--path"]).is_err());
        assert!(Cli::try_parse_from(["zync", "config", "--check"]).is_ok());
        assert!(Cli::try_parse_from(["zync", "config", "--path"]).is_ok());
    }

    #[test]
    fn plain_follow_scopes_to_the_current_session() {
        assert!(scope_to_session(true, false, None));
    }

    #[test]
    fn follow_with_an_explicit_line_count_shows_history_instead() {
        assert!(!scope_to_session(true, false, Some(100)));
    }

    #[test]
    fn explicit_session_wins_even_without_follow() {
        assert!(scope_to_session(false, true, None));
    }

    #[test]
    fn a_plain_tail_is_not_session_scoped() {
        assert!(!scope_to_session(false, false, None));
    }

    #[test]
    fn follow_and_session_together_is_still_session_scoped() {
        assert!(scope_to_session(true, true, Some(10)));
    }

    /// The service writes the log file; a client process must not also open it.
    #[test]
    fn only_the_service_and_a_foreground_run_write_the_log_file() {
        let writes_file = |command: Command| {
            matches!(command.logging(), Logging::Service | Logging::Foreground)
        };

        assert!(writes_file(Command::Daemon));
        assert!(writes_file(Command::Start { foreground: true }));
        assert!(!writes_file(Command::Start { foreground: false }));
        assert!(!writes_file(Command::Stop));
        assert!(!writes_file(Command::Status));
        assert!(!writes_file(Command::Config { check: false, path: false }));
        assert!(!writes_file(Command::Logs {
            follow: false,
            session: false,
            verbose: false,
            level: ops::LogLevel::Info,
            lines: None,
        }));
    }

    #[test]
    fn the_timestamp_is_dimmed_and_the_level_coloured() {
        let line = "2026-08-24T06:00:00.0Z ERROR zync::cli: something broke";
        let mut out = Vec::new();

        write_line(&mut out, line, true).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[2;3m2026-08-24T06:00:00.0Z\x1b[0m \x1b[31;1mERROR\x1b[0m zync::cli: something broke\n"
        );
    }

    #[test]
    fn color_disabled_writes_the_line_unchanged() {
        let line = "2026-08-24T06:00:00.0Z ERROR zync::cli: something broke";
        let mut out = Vec::new();

        write_line(&mut out, line, false).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), format!("{line}\n"));
    }

    /// A line with no readable level or timestamp — a panic, a wrapped line —
    /// must still be printed, just without colour, rather than dropped or
    /// mangled.
    #[test]
    fn a_line_without_a_recognised_shape_is_left_uncoloured() {
        let line = "thread 'main' panicked at src/lib.rs:1:1";
        let mut out = Vec::new();

        write_line(&mut out, line, true).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), format!("{line}\n"));
    }

    /// A short first word must not be mistaken for a timestamp — only genuine
    /// RFC 3339 tokens qualify.
    #[test]
    fn a_short_first_word_is_not_treated_as_a_timestamp() {
        assert!(!logline::looks_like_timestamp("thread"));
        assert!(logline::looks_like_timestamp("2026-08-24T06:00:00.0Z"));
    }

    #[test]
    fn every_written_level_has_a_distinct_colour() {
        use logline::Severity::{Debug, Error, Info, Trace, Warn};

        let codes: std::collections::HashSet<_> = [Trace, Debug, Info, Warn, Error]
            .into_iter()
            .map(color::level_code)
            .collect();

        assert_eq!(codes.len(), 5);
    }

    /// The formatter right-aligns levels to five columns, so INFO and WARN
    /// arrive with a leading space that has to survive colouring.
    #[test]
    fn the_level_padding_is_preserved() {
        let line = "2026-08-24T06:00:00.0Z  INFO zync::cli: hello";
        let mut out = Vec::new();

        write_line(&mut out, line, true).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\x1b[2;3m2026-08-24T06:00:00.0Z\x1b[0m  \x1b[32mINFO\x1b[0m zync::cli: hello\n"
        );
    }
}
