// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(any(feature = "acp", feature = "acp-http", feature = "tui"))]
pub(crate) fn init_file_logger() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let file = std::fs::File::create("zeph.log").ok();
    if let Some(file) = file {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(file)
            .with_line_number(true)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

#[cfg(all(not(feature = "tui"), feature = "otel"))]
use zeph_core::config::Config;

#[cfg(not(feature = "tui"))]
pub(crate) fn init_subscriber(config_path: &std::path::Path) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer();

    #[cfg(feature = "otel")]
    {
        let config = Config::load(config_path).ok();
        let use_otlp = config
            .as_ref()
            .is_some_and(|c| c.observability.exporter == "otlp");

        if use_otlp {
            let endpoint = config
                .as_ref()
                .map_or("http://localhost:4317", |c| &c.observability.endpoint);

            match setup_otel_tracer(endpoint) {
                Ok(tracer) => {
                    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(fmt_layer)
                        .with(otel_layer)
                        .init();
                    return;
                }
                Err(e) => {
                    eprintln!("OTel initialization failed, falling back to fmt: {e}");
                }
            }
        }
    }

    #[cfg(not(feature = "otel"))]
    let _ = config_path;

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}

#[cfg(all(feature = "otel", not(feature = "tui")))]
fn setup_otel_tracer(endpoint: &str) -> anyhow::Result<opentelemetry_sdk::trace::SdkTracer> {
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_otlp::WithExportConfig;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    let tracer = provider.tracer("zeph");
    opentelemetry::global::set_tracer_provider(provider);

    Ok(tracer)
}
