//! Entry point: start logging, parse arguments, hand off to the command.

use anyhow::{Context, Result};
use clap::Parser;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

mod cli;

/// A week of daily files. Long enough to look into a problem from a few evenings
/// ago, short enough that nobody has to think about disk usage.
const LOG_FILES_KEPT: usize = 7;

fn main() -> Result<()> {
    // Held for the life of the process: dropping it stops the background writer,
    // and anything not yet written is lost.
    let _guard = init_logging()?;

    cli::Cli::parse().command.run()
}

/// Logs to a rotating daily file, and to stderr so a foreground run still says
/// what it is doing. stdout is deliberately left clear for command output.
fn init_logging() -> Result<Option<WorkerGuard>> {
    // ZYNC_DEBUG predates this and is still documented, so it keeps working as a
    // shorthand for the debug level.
    let default = if std::env::var("ZYNC_DEBUG").is_ok_and(|value| value != "0") {
        "debug"
    } else {
        "info"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    let console = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .compact();

    // A missing log directory must not stop the app from running, so file
    // logging degrades to console-only rather than failing.
    let file = match open_log_file() {
        Ok(appender) => Some(tracing_appender::non_blocking(appender)),
        Err(e) => {
            eprintln!("warning: file logging disabled: {e:#}");
            None
        }
    };

    match file {
        Some((writer, guard)) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(console)
                .with(fmt::layer().with_writer(writer).with_ansi(false))
                .init();
            Ok(Some(guard))
        }
        None => {
            tracing_subscriber::registry().with(filter).with(console).init();
            Ok(None)
        }
    }
}

fn open_log_file() -> Result<RollingFileAppender> {
    let dir = zync_adapters::config::log_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;

    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("zync")
        .filename_suffix("log")
        .max_log_files(LOG_FILES_KEPT)
        .build(&dir)
        .with_context(|| format!("Failed to open a log file in {}", dir.display()))
}
