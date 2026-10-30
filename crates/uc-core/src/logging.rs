//! `tracing` setup: daily rolling files in the logs folder, keeping 7 days
//! (plan §2.11), plus stderr when a console is attached.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use crate::{Error, Result};

/// Days of logs to keep.
pub const KEEP_DAYS: usize = 7;

/// Options for [`init`].
#[derive(Debug, Clone)]
pub struct LogOptions<'a> {
    /// File name prefix, e.g. `"agent"` → `agent.2026-10-30.log`.
    pub prefix: &'a str,
    /// Filter in env-filter syntax (usually `config.general.log_level`).
    /// The `UC_LOG` environment variable overrides it.
    pub filter: &'a str,
    /// Also log to stderr (for `uc-probe` and debug runs).
    pub stderr: bool,
}

/// Installs the global subscriber. Keep the returned guard alive for the
/// life of the process; dropping it flushes and stops the file writer.
pub fn init(logs_dir: &Path, opts: &LogOptions<'_>) -> Result<WorkerGuard> {
    std::fs::create_dir_all(logs_dir).map_err(|e| Error::io(logs_dir, e))?;

    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(opts.prefix)
        .filename_suffix("log")
        .max_log_files(KEEP_DAYS)
        .build(logs_dir)
        .map_err(|e| Error::Logging(e.to_string()))?;
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter = std::env::var("UC_LOG")
        .ok()
        .and_then(|f| EnvFilter::try_new(f).ok())
        .or_else(|| EnvFilter::try_new(opts.filter).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));

    let file_layer = fmt::layer().with_writer(writer).with_ansi(false);
    let stderr_layer = opts
        .stderr
        .then(|| fmt::layer().with_writer(std::io::stderr).with_target(false));

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .map_err(|e| Error::Logging(e.to_string()))?;

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let guard = init(
            dir.path(),
            &LogOptions {
                prefix: "test",
                filter: "info",
                stderr: false,
            },
        )
        .unwrap();
        tracing::info!("hello from the test");
        drop(guard);

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "{files:?}");
        let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("test.") && name.ends_with(".log"),
            "{name}"
        );
        assert!(
            std::fs::read_to_string(&files[0])
                .unwrap()
                .contains("hello from the test")
        );
    }
}
