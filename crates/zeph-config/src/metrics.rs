// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Prometheus metrics export configuration (`[metrics]` TOML section).
//!
//! Controls whether the agent exposes a `/metrics` endpoint on the gateway HTTP server
//! and how frequently the internal `MetricsSnapshot` is synchronised to Prometheus counters
//! and gauges.
//!
//! # Example (TOML)
//!
//! ```toml
//! [metrics]
//! enabled = true
//! path = "/metrics"
//! sync_interval_secs = 5
//! ```

use serde::{Deserialize, Serialize};

fn default_metrics_path() -> String {
    "/metrics".into()
}

fn default_sync_interval_secs() -> u64 {
    5
}

/// Prometheus metrics export configuration.
///
/// When `enabled = true` and the binary is compiled with `--features prometheus`, the gateway
/// HTTP server mounts a `/metrics` (or configured `path`) route that returns `OpenMetrics` 1.0.0
/// text suitable for scraping by Prometheus.
///
/// Requires `[gateway] enabled = true`; if the gateway is disabled, metrics export is skipped
/// with a warning.
///
/// # Example (TOML)
///
/// ```toml
/// [metrics]
/// enabled = true
/// path = "/metrics"
/// sync_interval_secs = 5
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MetricsConfig {
    /// Whether Prometheus metrics export is active.
    ///
    /// When `false` (the default), no sync task is started and the `/metrics` route is not
    /// mounted even if the `prometheus` feature is compiled in.
    #[serde(default)]
    pub enabled: bool,

    /// HTTP path on which the `/metrics` endpoint is mounted.
    ///
    /// Must begin with `/`. Defaults to `"/metrics"`.
    #[serde(default = "default_metrics_path")]
    pub path: String,

    /// How often (in seconds) the `MetricsSnapshot` watch channel is read and synced to the
    /// Prometheus registry.
    ///
    /// Zero is clamped to `1` at runtime. Defaults to `5`.
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,

    /// Require the gateway bearer token on the `/metrics` endpoint. Default: `false`.
    ///
    /// When `false` (the default), `/metrics` is reachable without a bearer token and without
    /// rate limiting — matching the historical behavior and the common "scrape from a trusted
    /// network" deployment. Set to `true` to require the same `Authorization: Bearer <token>`
    /// header as `/webhook`, e.g. when the gateway is reachable from an untrusted network — this
    /// also gives `/metrics` its own independent per-IP rate-limit counter, with the same limit
    /// and the same middleware ordering as `/webhook` (rate limit outside auth, so a failed-auth
    /// request still counts toward the limit), since without it the bearer token would gain an
    /// unthrottled brute-force surface via `/metrics`.
    #[serde(default)]
    pub require_auth: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_metrics_path(),
            sync_interval_secs: default_sync_interval_secs(),
            require_auth: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_config_defaults() {
        let config: MetricsConfig = toml::from_str("").unwrap();
        assert!(!config.enabled);
        assert_eq!(config.path, "/metrics");
        assert_eq!(config.sync_interval_secs, 5);
        assert!(!config.require_auth);
    }

    #[test]
    fn test_metrics_config_serde() {
        let src = r#"
            enabled = true
            path = "/custom/metrics"
            sync_interval_secs = 10
            require_auth = true
        "#;
        let config: MetricsConfig = toml::from_str(src).unwrap();
        assert!(config.enabled);
        assert_eq!(config.path, "/custom/metrics");
        assert_eq!(config.sync_interval_secs, 10);
        assert!(config.require_auth);

        let serialized = toml::to_string(&config).unwrap();
        let roundtrip: MetricsConfig = toml::from_str(&serialized).unwrap();
        assert!(roundtrip.enabled);
        assert_eq!(roundtrip.path, "/custom/metrics");
        assert_eq!(roundtrip.sync_interval_secs, 10);
        assert!(roundtrip.require_auth);
    }
}
