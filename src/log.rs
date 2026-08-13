//! Where the logs go.
//!
//! The CLI logs to stderr and nowhere else. The two long-running halves also
//! write a rolling daily file, because by the time you want to know why a
//! session died, the terminal that started the server is long gone. Seven days
//! are kept, so this never grows without bound.
//!
//! `MM_LOG` overrides the filter for both, in the usual `env_filter` syntax.

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Files live under the platform's own log location so they turn up where
/// people look: Console.app on macOS, `~/.local/state` on Linux.
pub fn log_dir() -> PathBuf {
    if cfg!(target_os = "macos")
        && let Some(home) = dirs::home_dir()
    {
        return home.join("Library/Logs/manymux");
    }
    dirs::state_dir()
        .map(|dir| dir.join("manymux"))
        .unwrap_or_else(|| crate::config::config_dir().join("logs"))
}

/// The log directory belonging to `home`, for a system-wide unit that will run
/// as someone else. Same answer [`log_dir`] would give in that account, worked
/// out without their environment to ask.
pub fn log_dir_for(home: &std::path::Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Logs/manymux")
    } else {
        home.join(".local/state/manymux")
    }
}

/// Start logging. `file` names the rolling log file for a long-running process,
/// or `None` for a command that just prints and exits.
///
/// The returned guard flushes the file writer when it is dropped, so keep it
/// alive for as long as the process runs.
pub fn init(file: Option<&str>) -> Option<WorkerGuard> {
    let filter = |fallback: &str| {
        EnvFilter::try_from_env("MM_LOG").unwrap_or_else(|_| EnvFilter::new(fallback))
    };

    // The file keeps the detail; the console stays readable. The registry-wide
    // filter has to be the more permissive of the two, or events are dropped
    // before the file layer sees them.
    let console = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter("manymux=info"));

    let (files, guard) = match file.and_then(open_file) {
        Some(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer);
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter("info,manymux=debug"))
        .with(console)
        .with(files)
        .init();
    guard
}

/// A daily-rotating appender, or `None` with a warning if the directory is not
/// writable. Losing the log file is not a reason to refuse to start.
fn open_file(name: &str) -> Option<tracing_appender::rolling::RollingFileAppender> {
    let dir = log_dir();
    let built = std::fs::create_dir_all(&dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| {
            Ok(tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix(format!("{name}.log"))
                .max_log_files(7)
                .build(&dir)?)
        });
    match built {
        Ok(appender) => Some(appender),
        Err(e) => {
            eprintln!("mm: not logging to {}: {e:#}", dir.display());
            None
        }
    }
}
