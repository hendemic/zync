//! Entry point: parse arguments, start the right kind of logging, hand off.

use anyhow::{Context, Result};
use clap::Parser;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

mod cli;
mod service;

use cli::Logging;

/// A week of daily files. Long enough to look into a problem from a few evenings
/// ago, short enough that nobody has to think about disk usage.
const LOG_FILES_KEPT: usize = 7;

/// zbus logs a warning for every portal request whose object is already gone by
/// the time it tries to cache its properties. Harmless, and several lines of it
/// on every start, so it is quietened rather than left to bury the real output.
const NOISE: &str = "zbus=error";

fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    // Held for the life of the process: dropping it stops the background writer,
    // and anything not yet written is lost.
    let _guard = init_logging(cli.command.logging())?;

    cli.command.run()
}

/// Sets up logging for the kind of process this is.
///
/// Only one process writes the log file at a time — the service, or a foreground
/// run. Short-lived commands log to the terminal so their warnings are seen
/// without interleaving into the service's file.
fn init_logging(mode: Logging) -> Result<Option<WorkerGuard>> {
    if matches!(mode, Logging::Silent) {
        return Ok(None);
    }

    let console = matches!(mode, Logging::Foreground | Logging::Client).then(|| {
        fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .compact()
    });

    // A missing log directory must not stop the app from running, so file
    // logging degrades to console-only rather than failing.
    let file = match mode {
        Logging::Service | Logging::Foreground => match open_log_file() {
            Ok(appender) => Some(tracing_appender::non_blocking(appender)),
            Err(e) => {
                eprintln!("warning: file logging disabled: {e:#}");
                None
            }
        },
        _ => None,
    };

    let (file_layer, guard) = match file {
        Some((writer, guard)) => (
            Some(fmt::layer().with_writer(writer).with_ansi(false)),
            Some(guard),
        ),
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter())
        .with(console)
        .with(file_layer)
        .init();

    Ok(guard)
}

fn filter() -> EnvFilter {
    // ZYNC_DEBUG predates this and is still documented, so it keeps working as a
    // shorthand for the debug level.
    let level = if std::env::var("ZYNC_DEBUG").is_ok_and(|value| value != "0") {
        "debug"
    } else {
        "info"
    };

    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(format!("{level},{NOISE}")))
}

fn open_log_file() -> Result<RollingFileAppender> {
    let dir = zync_adapters::config::log_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;

    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("zync")
        .filename_suffix("log")
        .max_log_files(LOG_FILES_KEPT)
        .build(&dir)
        .with_context(|| format!("Failed to open a log file in {}", dir.display()))
}
