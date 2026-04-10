// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
///   feature). Falls back to the existing `[observability]` endpoint when `otlp_endpoint` is
///   unset.
/// - `Pyroscope`: continuous profiling via Pyroscope (requires the `profiling-pyroscope`
///   feature).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
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
    /// Include function arguments in span attributes. Set to `true` for local debugging.
    /// Keep `false` (the default) in production to avoid logging potentially sensitive data
    /// such as user messages, LLM responses, or tool outputs with PII.
    #[serde(default = "default_include_args")]
    pub include_args: bool,
    /// OTLP gRPC endpoint URL (used when `backend = "otlp"`).
    /// Falls back to `[observability].endpoint` when unset.
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
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
    /// Interval in seconds between system-metrics snapshots (Phase 3). Default: `5`.
    #[serde(default = "default_system_metrics_interval_secs")]
    pub system_metrics_interval_secs: u64,
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
            system_metrics_interval_secs: default_system_metrics_interval_secs(),
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
}
