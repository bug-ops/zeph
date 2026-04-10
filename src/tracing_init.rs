// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use zeph_core::config::{LogRotation, LoggingConfig, TelemetryConfig};

/// Guards that must be kept alive for the process lifetime.
///
/// Dropping any guard flushes and closes the corresponding writer.
/// Pass this struct to the top-level `run()` and hold it until the process exits.
pub(crate) struct TracingGuards {
    /// Async file-writer guard for the rolling log file. `None` when file logging is disabled.
    /// Held for its `Drop` side-effect (flushes the async file writer).
    #[allow(dead_code)]
    pub(crate) log_guard: Option<WorkerGuard>,
    /// Chrome trace flush guard. `None` when the `profiling` feature is absent or telemetry is
    /// disabled. Dropping this guard writes the final `]` to the JSON trace file.
    #[cfg(feature = "profiling")]
    #[allow(dead_code)]
    pub(crate) chrome_guard: Option<tracing_chrome::FlushGuard>,
}

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
/// Builds independent layers with separate filters and registers them in a single subscriber:
/// - stderr fmt layer controlled by `RUST_LOG` (default: `info`)
/// - optional file layer controlled by `logging.file` / `logging.level`
/// - optional Chrome JSON trace layer when `profiling` feature is enabled and
///   `telemetry.enabled = true` with `backend = "local"`
/// - optional `MetricsBridge` layer when `profiling` feature is enabled and
///   `metrics_collector` is `Some`
///
/// The CLI override and env vars must already be applied to `logging` before calling.
/// The returned [`TracingGuards`] **must** be held for the entire process lifetime;
/// dropping it flushes all async writers.
///
/// When `tui_mode` is true the stderr layer is omitted because ratatui owns
/// stdout (alternate screen) and any text written to stderr bleeds through
/// raw-mode, corrupting the TUI rendering. Logs still go to the file layer
/// when a log file is configured.
#[allow(clippy::too_many_lines)]
pub(crate) fn init_tracing(
    logging: &LoggingConfig,
    tui_mode: bool,
    telemetry: &TelemetryConfig,
    #[cfg(feature = "profiling")] metrics_collector: Option<
        std::sync::Arc<zeph_core::metrics::MetricsCollector>,
    >,
) -> TracingGuards {
    // Type alias for a boxed dynamic layer to allow composing heterogeneous layer types.
    type BoxedLayer =
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>;

    let mut layers: Vec<BoxedLayer> = Vec::new();

    // Stderr layer — omitted in TUI mode to avoid corrupting raw-mode rendering.
    if !tui_mode {
        let stderr_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        layers.push(Box::new(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(stderr_filter),
        ));
    }

    // Optional file layer.
    let mut log_guard: Option<WorkerGuard> = None;
    if !logging.file.is_empty() {
        let path = std::path::PathBuf::from(&logging.file);
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
            if !tui_mode {
                eprintln!("zeph: log directory creation failed, file logging disabled: {e}");
            }
        } else {
            let rotation = match logging.rotation {
                LogRotation::Daily => Rotation::DAILY,
                LogRotation::Hourly => Rotation::HOURLY,
                LogRotation::Never => Rotation::NEVER,
            };
            match RollingFileAppender::builder()
                .rotation(rotation)
                .max_log_files(logging.max_files)
                .filename_prefix(&filename_prefix)
                .filename_suffix(&filename_suffix)
                .build(&dir)
            {
                Err(e) => {
                    if !tui_mode {
                        eprintln!(
                            "zeph: log file appender init failed, file logging disabled: {e}"
                        );
                    }
                }
                Ok(appender) => {
                    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
                    let file_filter = tracing_subscriber::EnvFilter::try_new(&logging.level)
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
                    layers.push(Box::new(
                        tracing_subscriber::fmt::layer()
                            .with_writer(non_blocking)
                            .with_ansi(false)
                            .with_filter(file_filter),
                    ));
                    log_guard = Some(guard);
                }
            }
        }
    }

    // Optional Chrome JSON trace layer (compiled in only with the profiling feature).
    #[cfg(feature = "profiling")]
    let chrome_guard = build_chrome_layer(telemetry, &mut layers);

    // Optional MetricsBridge layer — derives TurnTimings from span durations.
    #[cfg(feature = "profiling")]
    if let Some(collector) = metrics_collector {
        layers.push(Box::new(zeph_core::metrics_bridge::MetricsBridge::new(
            collector,
        )));
    }

    // Suppress unused warning when profiling feature is disabled.
    #[cfg(not(feature = "profiling"))]
    let _ = telemetry;

    tracing_subscriber::registry().with(layers).init();

    TracingGuards {
        log_guard,
        #[cfg(feature = "profiling")]
        chrome_guard,
    }
}

/// Build the Chrome JSON trace layer and append it to `layers`.
///
/// Returns a `FlushGuard` that must be held until process exit.
/// Returns `None` when telemetry is disabled or backend is not `Local`.
#[cfg(feature = "profiling")]
fn build_chrome_layer(
    telemetry: &TelemetryConfig,
    layers: &mut Vec<
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>,
    >,
) -> Option<tracing_chrome::FlushGuard> {
    use zeph_core::config::TelemetryBackend;

    if !telemetry.enabled {
        return None;
    }

    if telemetry.backend == TelemetryBackend::Pyroscope {
        tracing::warn!(
            "telemetry backend 'pyroscope' is not yet implemented (Phase 4); no traces will be written"
        );
        return None;
    }

    if telemetry.backend != TelemetryBackend::Local {
        return None;
    }

    if let Err(e) = std::fs::create_dir_all(&telemetry.trace_dir) {
        eprintln!(
            "zeph: failed to create trace directory {}: {e}",
            telemetry.trace_dir.display()
        );
        return None;
    }

    let session_id = uuid::Uuid::new_v4().simple();
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S");
    let filename = format!("{session_id}_{timestamp}.json");
    let trace_path = telemetry.trace_dir.join(filename);

    let (chrome_layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
        .file(trace_path)
        .include_args(telemetry.include_args)
        .build();

    layers.push(Box::new(chrome_layer));
    Some(guard)
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

    /// Verify that `build_chrome_layer` returns `None` without creating files when telemetry
    /// is disabled, and that no layers are appended.
    #[cfg(feature = "profiling")]
    #[test]
    fn build_chrome_layer_disabled_returns_none() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let telemetry = TelemetryConfig {
            enabled: false,
            backend: TelemetryBackend::Local,
            trace_dir: std::path::PathBuf::from("/tmp/zeph-test-disabled"),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let guard = build_chrome_layer(&telemetry, &mut layers);
        assert!(guard.is_none(), "expected None when telemetry is disabled");
        assert!(
            layers.is_empty(),
            "no layer should be appended when disabled"
        );
    }

    /// Verify that `build_chrome_layer` returns a `FlushGuard` and creates a `.json` trace file
    /// when telemetry is enabled with `backend = Local`.
    #[cfg(feature = "profiling")]
    #[test]
    fn build_chrome_layer_enabled_local_creates_file() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let dir = tempfile::TempDir::new().expect("tempdir");
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Local,
            trace_dir: dir.path().to_path_buf(),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let guard = build_chrome_layer(&telemetry, &mut layers);
        assert!(
            guard.is_some(),
            "expected FlushGuard when telemetry is enabled"
        );
        assert_eq!(layers.len(), 1, "one chrome layer should be appended");
        // Drop the guard to flush and close the file.
        drop(guard);
        let json_files: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        assert!(
            !json_files.is_empty(),
            "expected at least one .json trace file"
        );
    }
}
