//! JSON `tracing` logs to `~/.local/state/s1/s1.log.<date>`, rotated daily,
//! 7 files kept. Logs hold metadata only, never payloads or keys.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::EnvFilter;

/// File name prefix of the log files.
pub const LOG_PREFIX: &str = "s1.log";
/// Number of daily files kept.
pub const LOG_DAYS: usize = 7;

/// Install the global JSON subscriber. Keep the guard alive until exit.
pub fn init(dir: &Path, level: &str) -> Result<WorkerGuard, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_PREFIX)
        .max_log_files(LOG_DAYS)
        .build(dir)
        .map_err(|e| format!("cannot open log in {}: {e}", dir.display()))?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("S1_LOG")
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_current_span(false)
        .try_init()
        .map_err(|e| format!("cannot install logger: {e}"))?;
    Ok(guard)
}

/// The newest log file in `dir`, if any.
pub fn latest_log(dir: &Path) -> Option<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(LOG_PREFIX))
        })
        .collect();
    files.sort();
    files.pop()
}
