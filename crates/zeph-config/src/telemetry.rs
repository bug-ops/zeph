// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn default_trace_dir() -> PathBuf {
    PathBuf::from(".local/traces")
}

fn default_include_args() -> bool {
    false
}

fn default_service_name() -> String {
    "zeph-agent".into()
}

fn default_sample_rate() -> f64 {
    1.0
}

fn default_system_metrics_interval_secs() -> u64 {
    5
}

/// Selects the tracing backend used when `[telemetry] enabled = true`.
///
/// - `Local`: writes Chrome JSON traces to `trace_dir` on disk.
/// - `Otlp`: exports spans to an OpenTelemetry collector via OTLP gRPC (requires the `otel`
///   feature). Uses `otlp_endpoint` (default: `"http://localhost:4317"`) when set.
/// - `Pyroscope`: continuous profiling via Pyroscope (requires the `profiling-pyroscope`
///   feature).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TelemetryBackend {
    /// Write `{trace_dir}/{session_id}_{timestamp}.json` Chrome traces.
    #[default]
    Local,
    /// Export spans via OTLP gRPC to an OpenTelemetry collector.
    Otlp,
    /// Push continuous CPU/memory profiles to a Pyroscope server.
    Pyroscope,
}

/// Profiling and distributed tracing configuration, nested under `[telemetry]` in TOML.
///
/// When `enabled = true` and the binary is compiled with `--features profiling`, agent turn
/// phases and LLM provider calls are instrumented with [`tracing`] spans. Traces are exported
/// according to the selected [`TelemetryBackend`].
///
/// Enabling telemetry has zero overhead when the `profiling` feature is absent — all
/// instrumentation points are compiled out via `cfg_attr`.
///
/// # Example (TOML)
///
/// ```toml
/// [telemetry]
/// enabled = true
/// backend = "local"
/// trace_dir = ".local/traces"
/// include_args = false
/// service_name = "my-zeph"
/// sample_rate = 0.1
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TelemetryConfig {
    /// Enable tracing instrumentation. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Backend to use for trace export. Default: `local`.
    #[serde(default)]
    pub backend: TelemetryBackend,
    /// Directory for Chrome JSON trace files (used when `backend = "local"`).
    /// Default: `".local/traces"`.
    #[serde(default = "default_trace_dir")]
    pub trace_dir: PathBuf,
    /// Include function arguments as span attributes in Chrome JSON traces.
    ///
    /// **Default: false.** Keep disabled in production — span field values are visible
    /// to all subscriber layers including OTLP. LLM prompts, tool outputs, and user
    /// messages may appear as span attributes if enabled.
    ///
    /// Note: this flag controls the Chrome JSON trace layer only, not OTLP span attributes.
    #[serde(default = "default_include_args")]
    pub include_args: bool,
    /// OTLP gRPC endpoint URL (used when `backend = "otlp"`).
    /// Default: `"http://localhost:4317"` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_endpoint: Option<String>,
    /// Vault key for OTLP authentication headers (e.g. `ZEPH_OTLP_HEADERS`).
    /// When set, the value is resolved from the age vault at startup and passed as
    /// `Authorization` or custom headers to the collector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otlp_headers_vault_key: Option<String>,
    /// Pyroscope server URL (used when `backend = "pyroscope"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pyroscope_endpoint: Option<String>,
    /// Service name reported in trace metadata. Default: `"zeph-agent"`.
    #[serde(default = "default_service_name")]
    pub service_name: String,
    /// Fraction of traces to sample. `1.0` = record all, `0.1` = record 10%.
    /// Applies only to the `otlp` backend; the `local` backend always records all spans.
    /// Default: `1.0`.
    ///
    /// # Warning
    ///
    /// `sample_rate` controls the fraction of completed traces sent to the OTLP collector,
    /// but the sampler runs **after** span creation. A low `sample_rate` reduces collector
    /// storage but provides **no protection** against CPU or RAM spikes caused by high span
    /// creation rates. Use [`otel_filter`][TelemetryConfig::otel_filter] (an `EnvFilter`
    /// applied before spans are created) to prevent the OTLP feedback loop.
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
    /// Optional base filter directive for the OTLP tracing layer.
    ///
    /// Accepts the same syntax as `RUST_LOG` / `EnvFilter` (e.g. `"info"`, `"debug,myapp=trace"`).
    /// When unset, defaults to `"info"`.
    ///
    /// # Hardcoded transport exclusions
    ///
    /// The following exclusions are **always appended** after the user-supplied value, regardless
    /// of what is set here:
    ///
    /// ```text
    /// tonic=warn,tower=warn,hyper=warn,h2=warn,opentelemetry=warn,rmcp=warn,sqlx=warn,want=warn
    /// ```
    ///
    /// `EnvFilter` uses last-directive-wins semantics, so these appended directives take
    /// precedence over any conflicting directive in this field. For example, setting
    /// `otel_filter = "tonic=debug"` will be silently overridden to `tonic=warn` because
    /// the hardcoded exclusion appears later in the filter string. This is intentional:
    /// allowing transport crates to emit `debug` spans would cause the OTLP exporter to
    /// capture its own network activity, creating a feedback loop.
    ///
    /// # Note on `sample_rate`
    ///
    /// `sample_rate` controls the fraction of traces sent to the OTLP collector, but the sampler
    /// runs **after** span creation. Setting `sample_rate < 1.0` reduces Jaeger storage but
    /// provides **no protection** against CPU or RAM spikes caused by high span creation rate.
    /// Only this `otel_filter` (an `EnvFilter` applied upstream of span creation) prevents
    /// the feedback loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otel_filter: Option<String>,
    /// Interval in seconds between system-metrics snapshots (Phase 3). Default: `5`.
    #[serde(default = "default_system_metrics_interval_secs")]
    pub system_metrics_interval_secs: u64,
    /// User-defined key/value pairs attached as OpenTelemetry resource attributes.
    ///
    /// These appear on every span exported via OTLP and in Chrome JSON trace
    /// `resourceSpans[].resource.attributes`. Useful for tagging traces with deployment
    /// environment, team, git revision, etc.
    ///
    /// Keys follow the [OpenTelemetry attribute naming convention](https://opentelemetry.io/docs/specs/semconv/general/attribute-naming/)
    /// (dot-separated, lowercase). The reserved key `service.name` is silently ignored —
    /// the `service_name` config field takes precedence.
    ///
    /// Values appear in plaintext in exported traces. The `RedactingSpanProcessor` does
    /// **not** scrub resource attributes (they are set once at init, not per-span). Do not
    /// store secrets here.
    ///
    /// # Example (TOML)
    ///
    /// ```toml
    /// [telemetry.trace_metadata]
    /// "deployment.environment" = "staging"
    /// "team.name" = "platform"
    /// "vcs.revision" = "abc1234"
    /// ```
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub trace_metadata: HashMap<String, String>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: TelemetryBackend::default(),
            trace_dir: default_trace_dir(),
            include_args: default_include_args(),
            otlp_endpoint: None,
            otlp_headers_vault_key: None,
            pyroscope_endpoint: None,
            service_name: default_service_name(),
            sample_rate: default_sample_rate(),
            otel_filter: None,
            system_metrics_interval_secs: default_system_metrics_interval_secs(),
            trace_metadata: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_config_defaults() {
        let cfg = TelemetryConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.backend, TelemetryBackend::Local);
        assert_eq!(cfg.trace_dir, PathBuf::from(".local/traces"));
        assert!(!cfg.include_args);
        assert!(cfg.otlp_endpoint.is_none());
        assert_eq!(cfg.service_name, "zeph-agent");
        assert!((cfg.sample_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn telemetry_config_serde_roundtrip() {
        let toml = r#"
            enabled = true
            backend = "otlp"
            trace_dir = "/tmp/traces"
            include_args = false
            otlp_endpoint = "http://otel:4317"
            service_name = "my-agent"
            sample_rate = 0.5
        "#;
        let cfg: TelemetryConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.backend, TelemetryBackend::Otlp);
        assert_eq!(cfg.trace_dir, PathBuf::from("/tmp/traces"));
        assert!(!cfg.include_args);
        assert_eq!(cfg.otlp_endpoint.as_deref(), Some("http://otel:4317"));
        assert_eq!(cfg.service_name, "my-agent");
        let serialized = toml::to_string(&cfg).unwrap();
        let cfg2: TelemetryConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg2.backend, TelemetryBackend::Otlp);
        assert_eq!(cfg2.service_name, "my-agent");
    }

    #[test]
    fn telemetry_config_old_toml_without_section_uses_defaults() {
        // Existing configs without [telemetry] must deserialize with defaults.
        let cfg: TelemetryConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.backend, TelemetryBackend::Local);
    }

    #[test]
    fn trace_metadata_parses_and_roundtrips() {
        let toml = r#"
            enabled = true
            backend = "otlp"
            service_name = "my-agent"

            [trace_metadata]
            "deployment.environment" = "staging"
            "team.name" = "platform"
        "#;
        let cfg: TelemetryConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.trace_metadata
                .get("deployment.environment")
                .map(String::as_str),
            Some("staging")
        );
        assert_eq!(
            cfg.trace_metadata.get("team.name").map(String::as_str),
            Some("platform")
        );

        // Roundtrip: serialize then deserialize preserves values.
        let serialized = toml::to_string(&cfg).unwrap();
        let cfg2: TelemetryConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(cfg2.trace_metadata, cfg.trace_metadata);
    }

    #[test]
    fn trace_metadata_empty_by_default() {
        let cfg = TelemetryConfig::default();
        assert!(cfg.trace_metadata.is_empty());
    }

    #[test]
    fn trace_metadata_omitted_when_empty_on_serialize() {
        let cfg = TelemetryConfig::default();
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(
            !serialized.contains("trace_metadata"),
            "empty trace_metadata must be omitted from serialized TOML"
        );
    }
}
