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

/// Hidden subcommand the detached child is launched with.
pub const DAEMON_COMMAND: &str = "__daemon";

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

/// Prints the tail of the current log file, optionally following it.
///
/// Rotation is handled by re-resolving the newest file as it goes, so a follow
/// running across midnight keeps working.
pub fn tail(lines: usize, follow: bool) -> Result<()> {
    let dir = config::log_dir()?;
    let mut path = newest_log(&dir)
        .ok_or_else(|| anyhow!("No log files yet in {}. Has zync run?", dir.display()))?;

    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let mut file = File::open(&path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut offset = print_tail(&mut file, lines, &mut out)?;
    out.flush()?;

    if !follow {
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

        offset = print_appended(&mut file, offset, &mut out)?;
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

fn print_tail(file: &mut File, lines: usize, out: &mut impl Write) -> Result<u64> {
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .context("Failed to read the log file")?;

    let all: Vec<&str> = contents.lines().collect();
    for line in all.iter().skip(all.len().saturating_sub(lines)) {
        writeln!(out, "{line}")?;
    }

    Ok(contents.len() as u64)
}

fn print_appended(file: &mut File, offset: u64, out: &mut impl Write) -> Result<u64> {
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
    write!(out, "{appended}")?;

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

    #[test]
    fn an_empty_directory_has_no_log() {
        let dir = TempDir::new("empty");

        assert!(newest_log(&dir.0).is_none());
    }
}
