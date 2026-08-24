//! Entry point: parse arguments, start the right kind of logging, hand off.

use anyhow::{Context, Result};
use clap::Parser;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

mod cli;
mod color;
mod service;

use cli::Logging;

/// A week of daily files. Long enough to look into a problem from a few evenings
/// ago, short enough that nobody has to think about disk usage.
const LOG_FILES_KEPT: usize = 7;

/// What reaches any layer at all.
///
/// Our own crates are recorded at debug so `zync logs -v` can answer questions
/// about a run that has already finished — verbosity becomes a reading decision
/// rather than something you must have predicted before starting.
///
/// Third-party crates stay at info: rumqttc and gstreamer at debug would bury
/// everything. zbus is quieter still, because it warns about property caching for
/// every portal request whose object has already gone away.
const DEFAULT_FILTER: &str =
    "info,zync=debug,zync_core=debug,zync_adapters=debug,zbus=error";

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
            // The terminal shows events; the file keeps the detail. A foreground
            // run that wants the detail on screen asks for it with ZYNC_DEBUG.
            .with_filter(console_level())
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
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// ZYNC_DEBUG predates all of this and is still documented, so it keeps working —
/// now as "show me the detail as it happens" rather than "record it".
fn console_level() -> LevelFilter {
    match std::env::var("ZYNC_DEBUG").is_ok_and(|value| value != "0") {
        true => LevelFilter::DEBUG,
        false => LevelFilter::INFO,
    }
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
