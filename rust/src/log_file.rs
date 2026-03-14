use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Default maximum size per log file in bytes (10 MB).
const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Default number of rotated log files to retain.
const DEFAULT_MAX_FILES: usize = 5;

/// Default filename prefix for log files.
const DEFAULT_PREFIX: &str = "symphony";

/// Configuration for rotating file logging.
pub struct LogFileConfig {
    /// Directory where log files are written.
    pub directory: PathBuf,
    /// Maximum size of a single log file in bytes before rotation.
    pub max_bytes: u64,
    /// Maximum number of rotated log files to keep.
    pub max_files: usize,
    /// Filename prefix used for log files.
    pub prefix: String,
}

impl LogFileConfig {
    /// Create a new configuration with defaults, using the given directory.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            max_bytes: DEFAULT_MAX_BYTES,
            max_files: DEFAULT_MAX_FILES,
            prefix: DEFAULT_PREFIX.to_string(),
        }
    }
}

/// Initialise tracing with both stdout and rotating file output.
///
/// The returned [`WorkerGuard`] **must** be held for the lifetime of the
/// application — dropping it flushes and stops the background writer thread.
pub fn init_file_logging(config: &LogFileConfig) -> WorkerGuard {
    // Ensure the log directory exists.
    let dir = Path::new(&config.directory);
    std::fs::create_dir_all(dir).unwrap_or_else(|e| {
        eprintln!(
            "Warning: could not create log directory {}: {e}",
            dir.display()
        );
    });

    // tracing-appender's RollingFileAppender handles rotation by time period.
    // We use daily rotation combined with a max of `max_files` retained logs.
    let file_appender = tracing_appender::rolling::Builder::new()
        .filename_prefix(config.prefix.as_str())
        .filename_suffix("log")
        .max_log_files(config.max_files)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .build(&config.directory)
        .expect("failed to create rolling file appender");

    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Stdout layer — compact, no targets (matches the previous default).
    let stdout_layer = fmt::layer()
        .with_target(false)
        .compact()
        .with_writer(std::io::stdout);

    // File layer — write to the non-blocking file appender.
    let file_layer = fmt::layer()
        .with_target(false)
        .with_ansi(false)
        .with_writer(non_blocking);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    guard
}
