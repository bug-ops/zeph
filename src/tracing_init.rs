// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use zeph_core::config::{LogRotation, LoggingConfig};

/// Resolve the effective log file path from CLI and config sources.
///
/// Priority: CLI `--log-file` > config `logging.file` > disabled (empty string → `None`).
/// An explicit empty CLI value disables file logging even if config has a path.
#[cfg(test)]
fn resolve_log_path(
    cli: Option<&std::path::Path>,
    config_file: &str,
) -> Option<std::path::PathBuf> {
    let file = match cli {
        Some(p) => p.to_string_lossy().into_owned(),
        None => config_file.to_owned(),
    };
    if file.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(file))
    }
}

/// Initialise the global tracing subscriber.
///
/// Builds two independent layers with separate filters:
/// - stderr fmt layer controlled by `RUST_LOG` (default: `info`)
/// - optional file layer controlled by `logging.file` / `logging.level`
///
/// The CLI override and env vars must already be applied to `logging` before calling.
/// The returned `WorkerGuard` **must** be held for the entire process lifetime;
/// dropping it flushes the async file writer.
pub(crate) fn init_tracing(logging: &LoggingConfig) -> Option<WorkerGuard> {
    let stderr_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer().with_filter(stderr_filter);

    let effective_path = if logging.file.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(&logging.file))
    };

    if let Some(path) = effective_path {
        let dir = path.parent().map_or_else(
            || std::path::PathBuf::from("."),
            std::path::Path::to_path_buf,
        );
        let filename_prefix = path
            .file_stem()
            .map_or_else(|| "zeph".to_owned(), |s| s.to_string_lossy().into_owned());
        let filename_suffix = path
            .extension()
            .map_or_else(|| "log".to_owned(), |s| s.to_string_lossy().into_owned());

        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("zeph: log directory creation failed, file logging disabled: {e}");
            tracing_subscriber::registry().with(stderr_layer).init();
            return None;
        }

        let rotation = match logging.rotation {
            LogRotation::Daily => Rotation::DAILY,
            LogRotation::Hourly => Rotation::HOURLY,
            LogRotation::Never => Rotation::NEVER,
        };

        let appender_result = RollingFileAppender::builder()
            .rotation(rotation)
            .max_log_files(logging.max_files)
            .filename_prefix(&filename_prefix)
            .filename_suffix(&filename_suffix)
            .build(&dir);

        let appender = match appender_result {
            Ok(a) => a,
            Err(e) => {
                eprintln!("zeph: log file appender init failed, file logging disabled: {e}");
                tracing_subscriber::registry().with(stderr_layer).init();
                return None;
            }
        };

        let (non_blocking, guard) = tracing_appender::non_blocking(appender);

        let file_filter = tracing_subscriber::EnvFilter::try_new(&logging.level)
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_filter(file_filter);

        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(file_layer)
            .init();

        Some(guard)
    } else {
        tracing_subscriber::registry().with(stderr_layer).init();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_log_path_no_cli_empty_config_returns_none() {
        assert!(resolve_log_path(None, "").is_none());
    }

    #[test]
    fn resolve_log_path_no_cli_config_set_returns_config_path() {
        let result = resolve_log_path(None, ".zeph/logs/zeph.log");
        assert_eq!(
            result.as_deref(),
            Some(std::path::Path::new(".zeph/logs/zeph.log"))
        );
    }

    #[test]
    fn resolve_log_path_cli_empty_disables_logging() {
        // Explicit empty CLI value overrides even a non-empty config.
        let result = resolve_log_path(Some(std::path::Path::new("")), ".zeph/logs/zeph.log");
        assert!(result.is_none());
    }

    #[test]
    fn resolve_log_path_cli_path_overrides_config() {
        let result = resolve_log_path(
            Some(std::path::Path::new("/tmp/custom.log")),
            ".zeph/logs/zeph.log",
        );
        assert_eq!(
            result.as_deref(),
            Some(std::path::Path::new("/tmp/custom.log"))
        );
    }
}
