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
// All fields intentionally share the `_guard` postfix to reflect their shared purpose.
#[allow(clippy::struct_field_names)]
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
    /// Pyroscope push guard. `None` when the `profiling-pyroscope` feature is absent,
    /// telemetry is disabled, or no endpoint is configured.
    /// Dropping this guard signals the background push task to stop.
    #[cfg(feature = "profiling-pyroscope")]
    #[allow(dead_code)]
    pub(crate) pyroscope_guard: Option<crate::pyroscope_push::PyroscopeGuard>,
    /// OTLP tracer provider shutdown handle. `None` when the `otel` feature is absent or
    /// telemetry backend is not `Otlp`. Dropping this guard flushes the `BatchSpanProcessor`
    /// queue and shuts down the provider cleanly.
    #[cfg(feature = "otel")]
    pub(crate) otel_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

// Drop order: otel_provider shuts down first (flushes pending spans),
// then chrome_guard, then log_guard. Rust drops struct fields in
// declaration order, so otel_provider must be declared last.
#[cfg(feature = "otel")]
impl Drop for TracingGuards {
    fn drop(&mut self) {
        if let Some(provider) = self.otel_provider.take()
            && let Err(e) = provider.shutdown()
        {
            eprintln!("zeph: OTLP provider shutdown error: {e}");
        }
    }
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
///
/// When `tui_mode` is true and no file log sink is configured, a warning is printed to
/// stderr before the TUI takes over, because the OTLP layer becomes the sole subscriber.
#[allow(clippy::too_many_lines)]
pub(crate) fn init_tracing(
    logging: &LoggingConfig,
    tui_mode: bool,
    telemetry: &TelemetryConfig,
    redact_secrets: bool,
    #[cfg(feature = "profiling")] metrics_collector: Option<
        std::sync::Arc<zeph_core::metrics::MetricsCollector>,
    >,
) -> TracingGuards {
    // Type alias for a boxed dynamic layer to allow composing heterogeneous layer types.
    type BoxedLayer =
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>;

    let mut layers: Vec<BoxedLayer> = Vec::new();

    // Determine whether OTLP will be the active trace sink.
    // When the `otel` feature is absent, OTLP is never active.
    #[cfg(feature = "otel")]
    let otlp_active = {
        use zeph_core::config::TelemetryBackend;
        telemetry.enabled && telemetry.backend == TelemetryBackend::Otlp
    };
    #[cfg(not(feature = "otel"))]
    let otlp_active = false;

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

    // In TUI mode the stderr layer is suppressed to avoid corrupting raw-mode rendering.
    // If logging.file was explicitly set to "" and no OTLP is configured the process would run
    // completely silent. Activate the platform default log path so traces are always reachable.
    if tui_mode && logging.file.is_empty() && !otlp_active {
        let fallback_path = std::path::PathBuf::from(zeph_core::config::default_log_file_path());
        let log_dir = fallback_path.parent().map_or_else(
            || std::path::PathBuf::from("."),
            std::path::Path::to_path_buf,
        );
        let filename = fallback_path.file_name().map_or_else(
            || "zeph.log".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        if let Err(e) = std::fs::create_dir_all(&log_dir) {
            eprintln!(
                "[zeph] warning: could not create fallback log directory {}: {e}",
                log_dir.display()
            );
        } else {
            let file_appender = tracing_appender::rolling::never(&log_dir, &filename);
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
            let fallback_filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
            layers.push(Box::new(
                tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_filter(fallback_filter),
            ));
            log_guard = Some(guard);
            eprintln!(
                "[zeph] info: TUI mode: no log sink configured, falling back to {}",
                fallback_path.display()
            );
        }
    }
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

    // Optional OTLP gRPC trace layer — active only when the `otel` feature is compiled in
    // AND `telemetry.backend == Otlp`. Layers are mutually selected by backend variant:
    // `build_chrome_layer` returns None for non-Local backends; `build_otlp_layer` activates
    // only for Otlp. Both can coexist in the layer vec without conflict.
    #[cfg(feature = "otel")]
    let otel_provider = build_otlp_layer(telemetry, &mut layers, true, redact_secrets);

    // Suppress unused warning when otel feature is inactive.
    #[cfg(not(feature = "otel"))]
    let _ = redact_secrets;

    // Optional MetricsBridge layer — derives TurnTimings from span durations.
    #[cfg(feature = "profiling")]
    if let Some(collector) = metrics_collector {
        layers.push(Box::new(zeph_core::metrics_bridge::MetricsBridge::new(
            collector,
        )));
    }

    // Optional AllocLayer — records per-span heap allocation counts and bytes.
    // Reads thread-local counters from CountingAllocator via the snapshot function pointer.
    #[cfg(feature = "profiling-alloc")]
    if telemetry.enabled {
        layers.push(Box::new(zeph_core::alloc_layer::AllocLayer::new(
            crate::alloc_counter::snapshot,
        )));
    }

    // Suppress unused warning when neither profiling nor otel features are active.
    #[cfg(not(any(feature = "profiling", feature = "otel")))]
    let _ = telemetry;

    tracing_subscriber::registry().with(layers).init();

    // Start Pyroscope continuous profiling push (after subscriber init so tracing works).
    #[cfg(feature = "profiling-pyroscope")]
    let pyroscope_guard = if telemetry.enabled {
        telemetry
            .pyroscope_endpoint
            .as_deref()
            .and_then(|ep| crate::pyroscope_push::start_pyroscope_push(ep, &telemetry.service_name))
    } else {
        None
    };

    TracingGuards {
        log_guard,
        #[cfg(feature = "profiling")]
        chrome_guard,
        #[cfg(feature = "profiling-pyroscope")]
        pyroscope_guard,
        #[cfg(feature = "otel")]
        otel_provider,
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

/// Build the OTLP gRPC trace layer and append it to `layers`.
///
/// Returns the `SdkTracerProvider` shutdown handle (stored in [`TracingGuards`]) or `None`
/// when telemetry is disabled or `telemetry.backend` is not `"otlp"`.
///
/// The `set_global` parameter controls whether `opentelemetry::global::set_tracer_provider` is
/// called. Pass `true` in production (`init_tracing`) and `false` in tests to avoid polluting
/// the global state and leaking `BatchSpanProcessor` background tasks.
///
/// The `redact_secrets` parameter controls whether a `RedactingSpanProcessor` wrapper is
/// inserted between the BSP and the exporter to scrub string attribute values before export.
///
/// # Panics
///
/// Does not panic. OTLP pipeline errors are logged via `tracing::warn!` and `None` is returned.
#[cfg(feature = "otel")]
fn build_otlp_layer(
    telemetry: &TelemetryConfig,
    layers: &mut Vec<
        Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static>,
    >,
    set_global: bool,
    redact_secrets: bool,
) -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{SpanExporter, WithExportConfig as _};
    use opentelemetry_sdk::trace::{
        BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracerProvider,
    };
    use tracing_subscriber::EnvFilter;
    use zeph_core::config::TelemetryBackend;

    if !telemetry.enabled || telemetry.backend != TelemetryBackend::Otlp {
        return None;
    }

    // All tracing::warn! calls inside this function fire before the subscriber is initialized
    // (subscriber.init() is called in init_tracing after this function returns). Use eprintln!
    // so diagnostic messages are not silently dropped.
    if telemetry.otlp_headers_vault_key.is_some() {
        eprintln!(
            "[zeph] warning: telemetry.otlp_headers_vault_key is set but not yet wired; \
             OTLP exporter connects unauthenticated"
        );
    }

    let endpoint = telemetry
        .otlp_endpoint
        .as_deref()
        .unwrap_or("http://localhost:4317");

    // #3001: warn when OTLP endpoint uses plaintext HTTP on a non-local host.
    if let Ok(url) = endpoint.parse::<url::Url>() {
        let host = url.host_str();
        // url::Url::host_str() returns IPv6 addresses with brackets: "[::1]".
        let is_local = host.is_none()
            || host == Some("localhost")
            || host == Some("127.0.0.1")
            || host == Some("[::1]");
        if url.scheme() == "http" && !is_local {
            eprintln!(
                "[zeph] warning: OTLP endpoint {endpoint} uses plaintext HTTP on a non-local host; \
                 consider using https:// to protect span data in transit"
            );
        }
    }

    let sample_rate = {
        let r = telemetry.sample_rate;
        if (0.0..=1.0).contains(&r) {
            r
        } else {
            let clamped = r.clamp(0.0, 1.0);
            eprintln!(
                "[zeph] warning: telemetry.sample_rate {r} is outside [0.0, 1.0]; clamping to {clamped}"
            );
            clamped
        }
    };

    // #2996: set a 3-second export timeout so the process does not block indefinitely
    // when the OTLP collector is unreachable.
    let exporter = match SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[zeph] warning: OTLP exporter init failed, tracing disabled: {e}");
            return None;
        }
    };

    // "service.name" is the canonical OTel semconv key (opentelemetry_semantic_conventions::resource::SERVICE_NAME).
    // We inline the string to avoid a new dependency on that crate.
    let resource = opentelemetry_sdk::Resource::builder_empty()
        .with_service_name(telemetry.service_name.clone())
        .build();

    // #2998: raise BSP queue size from the default 2048 to 4096 to absorb bursts during
    // high-throughput agent turns without dropping spans. This directly addresses the
    // CPU/RAM regression caused by unfiltered OTLP span creation (#2996).
    let batch_config = BatchConfigBuilder::default()
        .with_max_queue_size(4096)
        .build();
    // #3011: wrap with a circuit breaker to prevent CPU burn when the OTLP collector
    // is unavailable. After 3 consecutive export failures the circuit opens and spans
    // are silently dropped until the back-off window expires.
    let exporter = crate::circuit_breaker_exporter::CircuitBreakerExporter::new(exporter);
    let bsp = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();

    // #2999: optionally wrap BSP with a redacting processor to scrub string attributes.
    // Two builder paths avoid the `Box<dyn SpanProcessor>` indirection since
    // `SdkTracerProvider::with_span_processor` requires a concrete type bound.
    let provider = if redact_secrets {
        let redacting = crate::redacting_span_processor::RedactingSpanProcessor::new(bsp);
        SdkTracerProvider::builder()
            .with_span_processor(redacting)
            .with_sampler(Sampler::TraceIdRatioBased(sample_rate))
            .with_resource(resource)
            .build()
    } else {
        SdkTracerProvider::builder()
            .with_span_processor(bsp)
            .with_sampler(Sampler::TraceIdRatioBased(sample_rate))
            .with_resource(resource)
            .build()
    };

    if set_global {
        opentelemetry::global::set_tracer_provider(provider.clone());
    }

    // #2996: attach an EnvFilter to the OTLP layer to suppress transport-layer spans
    // (tonic, tower, hyper, h2, opentelemetry internal) from feeding back into the exporter,
    // which was the root cause of the 100% CPU / 20 GB RAM regression in TUI mode.
    let base = telemetry.otel_filter.as_deref().unwrap_or("info");
    let filter_str = format!(
        "{base},tonic=warn,tower=warn,hyper=warn,h2=warn,\
         opentelemetry=warn,rmcp=warn,sqlx=warn,want=warn"
    );
    let otel_filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::INFO.into())
        .parse_lossy(&filter_str);

    let tracer = provider.tracer(telemetry.service_name.clone());
    layers.push(Box::new(
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(otel_filter),
    ));

    Some(provider)
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

    /// Verify that `build_otlp_layer` returns `None` when telemetry is disabled, regardless of
    /// the backend setting, and that no layers are appended.
    #[cfg(feature = "otel")]
    #[test]
    fn build_otlp_layer_disabled_returns_none() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let telemetry = TelemetryConfig {
            enabled: false,
            backend: TelemetryBackend::Otlp,
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let provider = build_otlp_layer(&telemetry, &mut layers, false, false);
        assert!(
            provider.is_none(),
            "expected None when telemetry is disabled"
        );
        assert!(
            layers.is_empty(),
            "no layer should be appended when disabled"
        );
    }

    /// Verify that `build_otlp_layer` returns `None` when the backend is not Otlp.
    #[cfg(feature = "otel")]
    #[test]
    fn build_otlp_layer_non_otlp_backend_returns_none() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Local,
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let provider = build_otlp_layer(&telemetry, &mut layers, false, false);
        assert!(provider.is_none(), "expected None when backend is not Otlp");
        assert!(layers.is_empty(), "no layer should be appended");
    }

    /// Verify that the `sample_rate` clamp expression correctly bounds values to `[0.0, 1.0]`.
    /// The clamp logic runs before the network exporter is built — no live collector required.
    #[cfg(feature = "otel")]
    #[test]
    #[allow(clippy::float_cmp)]
    fn build_otlp_layer_sample_rate_out_of_range_is_clamped() {
        let clamp = |r: f64| {
            if (0.0..=1.0).contains(&r) {
                r
            } else {
                r.clamp(0.0, 1.0)
            }
        };
        assert_eq!(clamp(50.0), 1.0, "value > 1.0 must clamp to 1.0");
        assert_eq!(clamp(-0.5), 0.0, "negative value must clamp to 0.0");
        assert_eq!(
            clamp(0.5),
            0.5,
            "in-range value must pass through unchanged"
        );
        assert_eq!(clamp(0.0), 0.0, "boundary 0.0 must pass through unchanged");
        assert_eq!(clamp(1.0), 1.0, "boundary 1.0 must pass through unchanged");
    }

    /// Verify that the OTLP `EnvFilter` string is constructed correctly and suppresses
    /// transport-layer crates at `warn` level.
    ///
    /// Tests the filter construction logic from `build_otlp_layer` without requiring a live
    /// OTLP collector. Both the absence of parse errors and the presence of every exclusion
    /// directive are verified.
    ///
    /// Background: the absence of this filter was the root cause of the 100% CPU / 20 GB RAM
    /// regression in TUI mode (issue #2996). The feedback loop occurred because tonic/tower/hyper
    /// spans emitted during export were themselves captured by the OTLP layer.
    #[test]
    fn otlp_filter_suppresses_transport_crates() {
        use tracing_subscriber::EnvFilter;

        let base = "info";
        let filter_str = format!(
            "{base},tonic=warn,tower=warn,hyper=warn,h2=warn,\
             opentelemetry=warn,rmcp=warn,sqlx=warn,want=warn"
        );

        // Filter must parse without error.
        let filter = EnvFilter::builder()
            .with_default_directive(tracing::Level::INFO.into())
            .parse_lossy(&filter_str);

        // Verify all required exclusions are present in the formatted filter.
        let filter_repr = format!("{filter}");
        for crate_name in &[
            "tonic",
            "tower",
            "hyper",
            "h2",
            "opentelemetry",
            "rmcp",
            "sqlx",
            "want",
        ] {
            assert!(
                filter_repr.contains(crate_name),
                "filter missing exclusion for '{crate_name}': {filter_repr}"
            );
        }
    }

    /// Verify that the OTLP filter correctly merges a custom base directive with the hardcoded
    /// transport exclusions, and that the custom directive is preserved.
    #[test]
    fn otlp_filter_custom_base_preserved() {
        use tracing_subscriber::EnvFilter;

        let base = "debug,myapp=trace";
        let filter_str = format!(
            "{base},tonic=warn,tower=warn,hyper=warn,h2=warn,\
             opentelemetry=warn,rmcp=warn,sqlx=warn,want=warn"
        );

        // Must parse without panic even with a complex base.
        let filter = EnvFilter::builder()
            .with_default_directive(tracing::Level::INFO.into())
            .parse_lossy(&filter_str);

        let filter_repr = format!("{filter}");
        assert!(
            filter_repr.contains("tonic"),
            "tonic=warn must be present: {filter_repr}"
        );
        assert!(
            filter_repr.contains("myapp"),
            "custom base directive must be preserved: {filter_repr}"
        );
    }

    /// Verify the plaintext HTTP endpoint warning predicate used in `build_otlp_layer`.
    ///
    /// Tests the URL classification logic (local vs non-local, http vs https) that determines
    /// whether the `eprintln!` warning for unencrypted OTLP transport is emitted.
    #[test]
    fn plaintext_http_warning_predicate() {
        // Helper that mirrors the classification logic in build_otlp_layer.
        let should_warn = |endpoint: &str| -> bool {
            if let Ok(url) = endpoint.parse::<url::Url>() {
                let host = url.host_str();
                // url::Url::host_str() returns IPv6 addresses with brackets: "[::1]".
                let is_local = host.is_none()
                    || host == Some("localhost")
                    || host == Some("127.0.0.1")
                    || host == Some("[::1]");
                url.scheme() == "http" && !is_local
            } else {
                false
            }
        };

        // Local addresses must not warn even with http.
        assert!(
            !should_warn("http://localhost:4317"),
            "localhost http must not warn"
        );
        assert!(
            !should_warn("http://127.0.0.1:4317"),
            "loopback IPv4 http must not warn"
        );
        assert!(
            !should_warn("http://[::1]:4317"),
            "loopback IPv6 http must not warn"
        );

        // Non-local http must warn.
        assert!(
            should_warn("http://collector.internal:4317"),
            "non-local http must warn"
        );
        assert!(
            should_warn("http://10.0.0.5:4317"),
            "private IP http must warn"
        );

        // https must never warn regardless of host.
        assert!(
            !should_warn("https://collector.internal:4317"),
            "https must not warn"
        );
        assert!(
            !should_warn("https://localhost:4317"),
            "https localhost must not warn"
        );
    }

    /// Verify full `build_otlp_layer` pipeline with a live collector.
    /// Skipped in CI — run manually with Jaeger: `docker compose -f docker/docker-compose.tracing.yml up -d`
    #[cfg(feature = "otel")]
    #[test]
    #[ignore = "requires a live OTLP collector on localhost:4317"]
    fn build_otlp_layer_live_pipeline_returns_provider() {
        use zeph_core::config::{TelemetryBackend, TelemetryConfig};
        let telemetry = TelemetryConfig {
            enabled: true,
            backend: TelemetryBackend::Otlp,
            sample_rate: 1.0,
            otlp_endpoint: Some("http://localhost:4317".into()),
            ..TelemetryConfig::default()
        };
        let mut layers: Vec<
            Box<dyn tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync>,
        > = Vec::new();
        let provider = build_otlp_layer(&telemetry, &mut layers, false, false);
        assert!(provider.is_some(), "expected Some with valid endpoint");
        assert_eq!(layers.len(), 1, "one OTLP layer should be appended");
    }

    /// Verify that `TracingGuards` drops without panic when `otel_provider` is `Some`.
    /// Uses a no-exporter `SdkTracerProvider` (no network required).
    #[cfg(feature = "otel")]
    #[test]
    fn tracing_guards_drop_with_otel_provider_does_not_panic() {
        use opentelemetry_sdk::trace::SdkTracerProvider;
        let provider = SdkTracerProvider::builder().build();
        let guards = TracingGuards {
            log_guard: None,
            #[cfg(feature = "profiling")]
            chrome_guard: None,
            #[cfg(feature = "profiling-pyroscope")]
            pyroscope_guard: None,
            otel_provider: Some(provider),
        };
        drop(guards); // must not panic
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
            .filter_map(std::result::Result::ok)
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        assert!(
            !json_files.is_empty(),
            "expected at least one .json trace file"
        );
    }
}
