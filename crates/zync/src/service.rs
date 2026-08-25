//! Running zync as a detached background service, and looking in on it.
//!
//! The process model, rather than the sync itself: launching a child that
//! outlives the shell, recording it so `status` and `stop` can find it, and
//! following its log file.

use anyhow::{Context, Result, anyhow, bail};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use tracing::debug;
use zync_adapters::config;

use crate::color;

/// Hidden subcommand the detached child is launched with.
pub const DAEMON_COMMAND: &str = "__daemon";

/// Logged once when a session starts, and searched for by `logs --session`.
/// Both sides read it from here so rewording the message cannot break the flag.
pub const SESSION_MARKER: &str = "session starting";

/// How often `logs --follow` looks for new output.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(250);

/// Long enough for a bad config or an unreachable broker to have killed the
/// child, so `start` can say so instead of claiming success.
const STARTUP_GRACE: Duration = Duration::from_millis(750);

/// The pid of a running service, or `None` if nothing is running.
///
/// A pid file left behind by a killed process is treated as absent, so a crash
/// does not require manual cleanup.
pub fn running_pid() -> Option<u32> {
    let path = config::pid_path().ok()?;
    let pid = fs::read_to_string(&path).ok()?.trim().parse::<u32>().ok()?;

    if is_alive(pid) {
        Some(pid)
    } else {
        debug!(pid, "ignoring a stale pid file");
        let _ = fs::remove_file(&path);
        None
    }
}

/// Signal 0 asks the kernel whether a process exists without disturbing it.
/// `EPERM` means it exists but belongs to somebody else, which still counts.
#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 sends nothing; it only performs the existence
    // and permission check, and cannot affect the target process.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }

    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn is_alive(_pid: u32) -> bool {
    // Windows needs OpenProcess; until there is a capture backend for it, a
    // recorded pid is taken at face value.
    true
}

/// Records the running service, and removes the record when it exits.
pub struct PidFile {
    path: PathBuf,
}

impl PidFile {
    pub fn acquire() -> Result<Self> {
        if let Some(pid) = running_pid() {
            bail!("zync is already running (pid {pid})");
        }

        let path = config::pid_path()?;
        let dir = path.parent().context("Pid path has no parent directory")?;
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        fs::write(&path, std::process::id().to_string())
            .with_context(|| format!("Failed to write {}", path.display()))?;

        Ok(PidFile { path })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Launches the sync as a detached child and returns its pid.
///
/// The child is put in its own session so that closing the shell that started it
/// does not take it down, and its stdio is redirected away from the terminal.
pub fn spawn_detached() -> Result<u32> {
    let exe = std::env::current_exe().context("Could not locate the zync binary")?;
    let errors = open_stderr_log()?;

    let mut command = Command::new(exe);
    command
        .arg(DAEMON_COMMAND)
        .stdin(Stdio::null())
        .stdout(Stdio::from(errors.try_clone().context("Failed to redirect stdout")?))
        .stderr(Stdio::from(errors));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: setsid is async-signal-safe and is the documented way to
        // detach a child from its controlling terminal between fork and exec.
        unsafe {
            command.pre_exec(|| match libc::setsid() {
                -1 => Err(std::io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
    }

    let child = command.spawn().context("Failed to launch the zync service")?;
    let pid = child.id();

    // A child that dies immediately almost always means a bad config or an
    // unreachable broker, and saying "started" then would be a lie.
    thread::sleep(STARTUP_GRACE);
    if !is_alive(pid) {
        bail!(
            "zync exited immediately after starting. See {}",
            config::stderr_path()?.display()
        );
    }

    Ok(pid)
}

fn open_stderr_log() -> Result<File> {
    let path = config::stderr_path()?;
    let dir = path.parent().context("Log path has no parent directory")?;
    fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;

    File::options()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open {}", path.display()))
}

/// Waits for a stopping service to actually exit, so `stop` can report what
/// happened rather than just that the request was sent.
pub fn await_exit(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;

    while std::time::Instant::now() < deadline {
        if running_pid().is_none() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }

    running_pid().is_none()
}

/// A threshold for what `logs` prints, applied to what the file already holds.
///
/// The file records our own crates at debug, so this is a read-time choice: a
/// problem can be examined in detail after the fact without restarting anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
#[clap(rename_all = "lower")]
pub enum LogLevel {
    /// Everything in the file. Identical to `debug` unless `RUST_LOG` was
    /// widened when the service started, since nothing here logs at trace.
    #[value(alias = "trace")]
    All,
    /// Per-frame detail: frame rate, frames delivered, per-zone colour and pacing.
    Debug,
    /// Events: started, connected, stopped, lights restored, problems.
    Info,
    /// Warnings and errors.
    Warn,
    /// Errors only.
    Error,
}

impl LogLevel {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "TRACE" => Some(LogLevel::All),
            "DEBUG" => Some(LogLevel::Debug),
            "INFO" => Some(LogLevel::Info),
            "WARN" => Some(LogLevel::Warn),
            "ERROR" => Some(LogLevel::Error),
            _ => None,
        }
    }
}

/// How `logs` was asked to narrow things down.
pub struct LogView {
    pub level: LogLevel,
    /// Show only what the most recent session logged.
    pub session: bool,
    /// How many lines of history, ignored when `session` is set.
    pub lines: usize,
    pub follow: bool,
}

/// The formatter writes `<timestamp> <LEVEL> <target>: <message>`, so the level
/// is the second token. Lines it cannot be read from are kept rather than
/// hidden — a line of unexpected shape is more likely to matter, not less.
fn included(line: &str, threshold: LogLevel) -> bool {
    line.split_whitespace()
        .nth(1)
        .and_then(LogLevel::parse)
        .is_none_or(|level| level >= threshold)
}

/// Narrows a file's lines to what was asked for.
fn selected<'a>(contents: &'a str, view: &LogView) -> Vec<&'a str> {
    let lines: Vec<&str> = contents.lines().collect();

    // Scoping to a session comes first: the marker is logged at info, so a
    // debug view of one session still starts in the right place.
    let start = match view.session {
        true => lines
            .iter()
            .rposition(|line| line.contains(SESSION_MARKER))
            .unwrap_or(0),
        false => 0,
    };

    let kept: Vec<&str> = lines[start..]
        .iter()
        .copied()
        .filter(|line| included(line, view.level))
        .collect();

    // A session is shown whole; otherwise the tail is what was asked for.
    match view.session {
        true => kept,
        false => kept[kept.len().saturating_sub(view.lines)..].to_vec(),
    }
}

/// Prints the tail of the current log file, optionally following it.
///
/// Rotation is handled by re-resolving the newest file as it goes, so a follow
/// running across midnight keeps working.
pub fn tail(view: LogView) -> Result<()> {
    let dir = config::log_dir()?;
    let mut path = newest_log(&dir)
        .ok_or_else(|| anyhow!("No log files yet in {}. Has zync run?", dir.display()))?;

    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let mut file = File::open(&path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut offset = print_history(&mut file, &view, &mut out)?;
    out.flush()?;

    if !view.follow {
        return Ok(());
    }

    loop {
        thread::sleep(FOLLOW_INTERVAL);

        // Midnight rotation moves the writer to a new file, so re-resolve rather
        // than follow a file nothing is appending to any more.
        if let Some(newest) = newest_log(&dir)
            && newest != path
        {
            path = newest;
            file =
                File::open(&path).with_context(|| format!("Failed to open {}", path.display()))?;
            offset = 0;
        }

        offset = print_appended(&mut file, offset, view.level, &mut out)?;
        out.flush()?;
    }
}

/// The log file currently being written, for `status` to point at.
pub fn current_log() -> Option<PathBuf> {
    newest_log(&config::log_dir().ok()?)
}

/// The appender writes `zync.<date>.log`, so with a fixed prefix and suffix the
/// lexical maximum is the current file.
///
/// The prefix match has to be exact: `stderr.log` shares the directory, and
/// anything matching it loosely would sort above every dated file.
fn newest_log(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("zync.") && name.ends_with(".log"))
        })
        .max()
}

/// The formatter's timestamp shape: RFC 3339 with a literal `Z`, e.g.
/// `2026-08-24T06:42:01.006965Z`. Long enough, and specific enough, that the
/// first word of a panic or another unrelated line will never satisfy it.
fn looks_like_timestamp(token: &str) -> bool {
    token.len() >= 20 && token.contains('T') && token.ends_with('Z')
}

/// Writes one line: the timestamp dimmed and italicised, the level token
/// coloured by severity, everything else — target, message — passed through
/// untouched.
///
/// Both are found positionally rather than reconstructed from
/// `split_whitespace`: the timestamp, if present, is the line's first token,
/// and the level is the first token after it that matches a known severity. A
/// line that is not shaped this way prints as-is rather than being guessed at.
fn write_line(out: &mut impl Write, line: &str, color: bool) -> Result<()> {
    if !color {
        writeln!(out, "{line}")?;
        return Ok(());
    }

    let mut prefix = String::new();
    let mut rest = line;

    if let Some(ts_end) = line.find(char::is_whitespace) {
        let candidate = &line[..ts_end];
        if looks_like_timestamp(candidate) {
            prefix = color::dim_italic(candidate, true);
            rest = &line[ts_end..];
        }
    }

    let level = rest
        .split_whitespace()
        .next()
        .and_then(|token| color::level_code(token).map(|code| (token, code)))
        .and_then(|(token, code)| rest.find(token).map(|at| (at, token, code)));

    match level {
        Some((at, token, code)) => writeln!(
            out,
            "{prefix}{}\x1b[{code}m{token}\x1b[0m{}",
            &rest[..at],
            &rest[at + token.len()..]
        )?,
        None => writeln!(out, "{prefix}{rest}")?,
    }

    Ok(())
}

fn print_history(file: &mut File, view: &LogView, out: &mut impl Write) -> Result<u64> {
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .context("Failed to read the log file")?;

    let color = color::enabled();
    for line in selected(&contents, view) {
        write_line(out, line, color)?;
    }

    Ok(contents.len() as u64)
}

fn print_appended(
    file: &mut File,
    offset: u64,
    level: LogLevel,
    out: &mut impl Write,
) -> Result<u64> {
    let length = file.metadata().context("Failed to stat the log file")?.len();

    // A shorter file means it was rotated or truncated underneath us.
    let start = if length < offset { 0 } else { offset };
    if length == start {
        return Ok(start);
    }

    file.seek(SeekFrom::Start(start))?;
    let mut appended = String::new();
    file.read_to_string(&mut appended)
        .context("Failed to read the log file")?;

    let color = color::enabled();
    for line in appended.lines().filter(|line| included(line, level)) {
        write_line(out, line, color)?;
    }

    Ok(start + appended.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("zync-test-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn touch(&self, name: &str) {
            File::create(self.0.join(name)).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn view(level: LogLevel, session: bool, lines: usize) -> LogView {
        LogView { level, session, lines, follow: false }
    }

    const SAMPLE: &str = "\
2026-08-24T06:00:00.0Z  INFO zync::cli: session starting instance=\"a\"
2026-08-24T06:00:01.0Z DEBUG zync_core::app: capture rate fps=12.05
2026-08-24T06:00:02.0Z  WARN zync_adapters::mqtt: something odd
2026-08-24T06:10:00.0Z  INFO zync::cli: session starting instance=\"b\"
2026-08-24T06:10:01.0Z DEBUG zync_core::app: capture rate fps=11.9
2026-08-24T06:10:02.0Z  INFO zync_core::app: session stopped";

    #[test]
    fn the_default_view_hides_debug_detail() {
        let shown = selected(SAMPLE, &view(LogLevel::Info, false, 100));

        assert_eq!(shown.len(), 4, "two debug lines should be dropped");
        assert!(shown.iter().all(|line| !line.contains("capture rate")));
    }

    #[test]
    fn a_debug_view_shows_everything_already_recorded() {
        assert_eq!(selected(SAMPLE, &view(LogLevel::Debug, false, 100)).len(), 6);
    }

    /// `all` and `trace` name the same threshold, and both must reach it.
    #[test]
    fn all_is_the_lowest_threshold_and_accepts_trace_as_a_name() {
        use clap::ValueEnum;

        assert_eq!(LogLevel::from_str("all", false), Ok(LogLevel::All));
        assert_eq!(LogLevel::from_str("trace", false), Ok(LogLevel::All));
        assert!(LogLevel::All < LogLevel::Debug);
        assert_eq!(
            selected(SAMPLE, &view(LogLevel::All, false, 100)).len(),
            selected(SAMPLE, &view(LogLevel::Debug, false, 100)).len(),
            "nothing here logs at trace, so the two agree"
        );
    }

    #[test]
    fn a_warn_view_keeps_only_problems() {
        let shown = selected(SAMPLE, &view(LogLevel::Warn, false, 100));

        assert_eq!(shown, vec!["2026-08-24T06:00:02.0Z  WARN zync_adapters::mqtt: something odd"]);
    }

    /// The point of --session: the previous run's lines must not appear.
    #[test]
    fn a_session_view_starts_at_the_last_session_marker() {
        let shown = selected(SAMPLE, &view(LogLevel::Debug, true, 1));

        assert_eq!(shown.len(), 3);
        assert!(shown[0].contains("instance=\"b\""), "should start at the newer session");
        assert!(!shown.iter().any(|line| line.contains("fps=12.05")));
    }

    /// A session is shown whole, so -n must not clip it.
    #[test]
    fn a_session_view_ignores_the_line_count() {
        let all = selected(SAMPLE, &view(LogLevel::Debug, true, 1));

        assert_eq!(all.len(), 3);
    }

    #[test]
    fn the_line_count_applies_to_the_tail_when_not_scoped() {
        let shown = selected(SAMPLE, &view(LogLevel::Debug, false, 2));

        assert_eq!(shown.len(), 2);
        assert!(shown[1].contains("session stopped"), "should keep the newest lines");
    }

    /// A file with no marker yet must still show something rather than nothing.
    #[test]
    fn a_session_view_without_a_marker_falls_back_to_the_whole_file() {
        let contents = "2026-08-24T06:00:00.0Z  INFO zync::cli: no marker here";

        assert_eq!(selected(contents, &view(LogLevel::Info, true, 10)).len(), 1);
    }

    /// A panic or a wrapped line has no level token; hiding it would lose exactly
    /// the output most worth seeing.
    #[test]
    fn lines_without_a_level_are_kept() {
        let contents = "thread 'main' panicked at src/lib.rs:1:1";

        assert_eq!(selected(contents, &view(LogLevel::Error, false, 10)).len(), 1);
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
        assert!(!looks_like_timestamp("thread"));
        assert!(looks_like_timestamp("2026-08-24T06:00:00.0Z"));
    }

    #[test]
    fn every_written_level_has_a_distinct_colour() {
        let codes: std::collections::HashSet<_> =
            ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"]
                .iter()
                .map(|level| color::level_code(level).unwrap())
                .collect();

        assert_eq!(codes.len(), 5);
    }

    /// Regression: the appender writes `zync.<date>.log`, and an earlier filter
    /// looked for `zync.log.<date>`, which matched nothing at all.
    #[test]
    fn the_newest_dated_log_is_chosen() {
        let dir = TempDir::new("newest");
        dir.touch("zync.2026-08-23.log");
        dir.touch("zync.2026-08-24.log");

        let newest = newest_log(&dir.0).unwrap();

        assert_eq!(newest.file_name().unwrap(), "zync.2026-08-24.log");
    }

    /// `stderr.log` shares the directory and would sort above any date, so a
    /// loose match would make `zync logs` follow the wrong file forever.
    #[test]
    fn the_stderr_log_is_not_mistaken_for_the_newest_log() {
        let dir = TempDir::new("stderr");
        dir.touch("zync.2026-08-24.log");
        dir.touch("stderr.log");

        assert_eq!(
            newest_log(&dir.0).unwrap().file_name().unwrap(),
            "zync.2026-08-24.log"
        );
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

    #[test]
    fn an_empty_directory_has_no_log() {
        let dir = TempDir::new("empty");

        assert!(newest_log(&dir.0).is_none());
    }
}
