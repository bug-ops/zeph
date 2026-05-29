// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration for Context-Adaptive Memory (CAM) fidelity scoring.
//!
//! [`FidelityConfig`] is serialised from the `[context.fidelity]` section in `config.toml`.
//! When `enabled = false` (the default) the fidelity scorer is a complete no-op.

use serde::{Deserialize, Serialize};

fn fidelity_lookahead_depth_default() -> u8 {
    FidelityConfig::default_lookahead_depth()
}

/// Configuration for the heuristic fidelity scorer (CAM §8.1).
///
/// All weight fields must be positive. Weights are normalised at runtime by
/// the sum of active weights (INV-05).
///
/// # Examples
///
/// ```
/// use zeph_config::fidelity::FidelityConfig;
///
/// let cfg = FidelityConfig::default();
/// assert!(!cfg.enabled, "fidelity scoring is off by default");
/// assert!((cfg.w_semantic - 0.3).abs() < f32::EPSILON);
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct FidelityConfig {
    /// Master switch. When `false`, no fidelity scoring occurs.
    pub enabled: bool,
    /// Cosine/keyword semantic relevance weight.
    ///
    /// Previously named `w_keyword` in config — that name is still accepted for compatibility.
    #[serde(alias = "w_keyword")]
    pub w_semantic: f32,
    /// Recency weight.
    pub w_temporal: f32,
    /// Role-based importance weight.
    pub w_importance: f32,
    /// Plan-hint relevance weight (active only when `planned_tools` is non-empty).
    pub w_plan: f32,
    /// Score threshold above which a message retains `Full` fidelity.
    pub full_threshold: f32,
    /// Score threshold above which a message is `Compressed` (not `Placeholder`).
    pub compressed_threshold: f32,
    /// Maximum tokens kept when rendering a `Compressed` message.
    pub compressed_max_tokens: usize,
    /// Budget ratio at which `AgeMem` triggers a proactive regrade.
    pub regrade_threshold: f32,
    /// Minimum query length for semantic signal to be active.
    pub min_query_length: usize,
    /// Maximum number of messages scored per turn (performance cap).
    pub max_scored_messages: usize,
    /// Number of the newest messages exempt from scoring when the window exceeds
    /// `max_scored_messages`. These messages default to `Full` fidelity.
    ///
    /// A value of `0` (the default) means no tail exemption beyond the hard
    /// `max_scored_messages` cap.
    #[serde(default)]
    pub exempt_tail_messages: usize,
    /// LLM provider name (from `[[llm.providers]]`) used to summarize messages during
    /// `Compressed` rendering. When `None`, truncation is used instead.
    #[serde(default)]
    pub compress_provider: Option<String>,
    /// Embedding provider name (from `[[llm.providers]]`) used for semantic similarity scoring.
    /// When `None`, keyword overlap is used instead.
    #[serde(default)]
    pub semantic_scoring_provider: Option<String>,
    /// Maximum BFS depth for PAACE lookahead hints derived from the orchestration DAG.
    ///
    /// Controls how many steps ahead in the active task graph are converted to
    /// `PlannedToolHint` values and passed to `FidelityScorer`.
    /// `0` disables lookahead (returns an empty hint slice). Valid range: `0..=5`.
    #[serde(default = "fidelity_lookahead_depth_default")]
    pub lookahead_depth: u8,
    /// Maximum number of concurrent `provider.embed()` calls during the cold-start pre-pass.
    ///
    /// Controls the `buffer_unordered(N)` bound. Higher values reduce latency on cold starts
    /// at the cost of more concurrent API requests. Default is `32`.
    #[serde(default = "default_embed_concurrency")]
    pub embed_concurrency: usize,
    /// Hard cap on message content length (in approximate tokens) fed to `provider.embed()`.
    ///
    /// When `Some(n)`, message content is truncated to approximately `n * 4` characters
    /// (at a valid UTF-8 char boundary) before the embed call. `None` means no cap.
    #[serde(default)]
    pub max_embed_input_tokens: Option<usize>,
    /// Hard cap on message content length (in approximate tokens) fed to the LLM compress call.
    ///
    /// When `Some(n)`, the input is truncated to approximately `n * 4` characters before
    /// the compress call. `None` means no cap. Independent of the existing 2× cost guard.
    #[serde(default)]
    pub max_compress_input_tokens: Option<usize>,
}

fn default_embed_concurrency() -> usize {
    32
}

impl FidelityConfig {
    /// Default value for [`lookahead_depth`](FidelityConfig::lookahead_depth): 3 BFS steps.
    ///
    /// Used as the `serde` default function and for callers that need the fallback value without
    /// constructing a full [`FidelityConfig`].
    #[must_use]
    pub fn default_lookahead_depth() -> u8 {
        3
    }

    /// Validate threshold ordering: `full_threshold >= compressed_threshold >= 0.0`.
    ///
    /// Call this at config load time to catch inverted thresholds before they silently
    /// misclassify messages (score in `compressed_threshold..full_threshold` becomes Full
    /// instead of Compressed when the invariant is violated).
    ///
    /// # Errors
    ///
    /// Returns an error string describing the violated constraint.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_config::fidelity::FidelityConfig;
    ///
    /// let valid = FidelityConfig::default();
    /// assert!(valid.validate().is_ok());
    ///
    /// let invalid = FidelityConfig { full_threshold: 0.2, compressed_threshold: 0.5, ..FidelityConfig::default() };
    /// assert!(invalid.validate().is_err());
    /// ```
    pub fn validate(&self) -> Result<(), String> {
        if self.compressed_threshold < 0.0 {
            return Err("context.fidelity: compressed_threshold must be >= 0.0".into());
        }
        if self.full_threshold > 1.0 {
            return Err("context.fidelity: full_threshold must be <= 1.0".into());
        }
        if self.full_threshold < self.compressed_threshold {
            return Err(format!(
                "context.fidelity: full_threshold ({}) must be >= compressed_threshold ({})",
                self.full_threshold, self.compressed_threshold
            ));
        }
        if self.lookahead_depth > 5 {
            return Err(format!(
                "context.fidelity: lookahead_depth ({}) must be <= 5",
                self.lookahead_depth
            ));
        }
        Ok(())
    }
}

impl Default for FidelityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            w_semantic: 0.3,
            w_temporal: 0.3,
            w_importance: 0.2,
            w_plan: 0.2,
            full_threshold: 0.7,
            compressed_threshold: 0.3,
            compressed_max_tokens: 50,
            regrade_threshold: 0.6,
            min_query_length: 8,
            max_scored_messages: 500,
            exempt_tail_messages: 0,
            compress_provider: None,
            semantic_scoring_provider: None,
            lookahead_depth: Self::default_lookahead_depth(),
            embed_concurrency: default_embed_concurrency(),
            max_embed_input_tokens: None,
            max_compress_input_tokens: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_disabled() {
        let cfg = FidelityConfig::default();
        assert!(!cfg.enabled);
    }

    #[test]
    fn deserialize_enabled() {
        let toml_str = r"
            enabled = true
            w_semantic = 0.4
            regrade_threshold = 0.7
        ";
        let cfg: FidelityConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.enabled);
        assert!((cfg.w_semantic - 0.4).abs() < f32::EPSILON);
        assert!((cfg.regrade_threshold - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn deserialize_w_keyword_alias() {
        let toml_str = r"
            enabled = true
            w_keyword = 0.25
        ";
        let cfg: FidelityConfig = toml::from_str(toml_str).unwrap();
        assert!((cfg.w_semantic - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn deserialize_semantic_scoring_provider() {
        let toml_str = r#"
            enabled = true
            semantic_scoring_provider = "embed-fast"
        "#;
        let cfg: FidelityConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.semantic_scoring_provider.as_deref(), Some("embed-fast"));
    }

    #[test]
    fn deserialize_defaults_for_omitted_fields() {
        let cfg: FidelityConfig = toml::from_str("enabled = false").unwrap();
        assert!((cfg.w_temporal - 0.3).abs() < f32::EPSILON);
        assert_eq!(cfg.compressed_max_tokens, 50);
        assert_eq!(cfg.max_scored_messages, 500);
    }

    #[test]
    fn validate_defaults_ok() {
        assert!(FidelityConfig::default().validate().is_ok());
    }

    #[test]
    fn validate_inverted_thresholds_err() {
        let cfg = FidelityConfig {
            full_threshold: 0.2,
            compressed_threshold: 0.5,
            ..FidelityConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.contains("full_threshold"),
            "error should mention full_threshold: {err}"
        );
    }

    #[test]
    fn validate_negative_compressed_threshold_err() {
        let cfg = FidelityConfig {
            compressed_threshold: -0.1,
            ..FidelityConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_full_threshold_above_one_err() {
        let cfg = FidelityConfig {
            full_threshold: 1.1,
            ..FidelityConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn default_lookahead_depth_is_three() {
        assert_eq!(FidelityConfig::default().lookahead_depth, 3);
    }

    #[test]
    fn lookahead_depth_zero_is_valid() {
        let cfg = FidelityConfig {
            lookahead_depth: 0,
            ..FidelityConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn lookahead_depth_five_is_valid() {
        let cfg = FidelityConfig {
            lookahead_depth: 5,
            ..FidelityConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn lookahead_depth_above_five_is_err() {
        let cfg = FidelityConfig {
            lookahead_depth: 6,
            ..FidelityConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.contains("lookahead_depth"),
            "error should mention lookahead_depth: {err}"
        );
    }

    #[test]
    fn deserialize_lookahead_depth() {
        let toml_str = "enabled = true\nlookahead_depth = 2";
        let cfg: FidelityConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.lookahead_depth, 2);
    }

    #[test]
    fn deserialize_defaults_lookahead_depth_when_omitted() {
        let cfg: FidelityConfig = toml::from_str("enabled = false").unwrap();
        assert_eq!(cfg.lookahead_depth, 3);
    }

    #[test]
    fn deserialize_new_perf_fields_defaults() {
        let cfg: FidelityConfig = toml::from_str("enabled = false").unwrap();
        assert_eq!(cfg.embed_concurrency, 32);
        assert!(cfg.max_embed_input_tokens.is_none());
        assert!(cfg.max_compress_input_tokens.is_none());
    }

    #[test]
    fn deserialize_new_perf_fields_custom() {
        let toml_str = r"
            enabled = true
            embed_concurrency = 8
            max_embed_input_tokens = 512
            max_compress_input_tokens = 1024
        ";
        let cfg: FidelityConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.embed_concurrency, 8);
        assert_eq!(cfg.max_embed_input_tokens, Some(512));
        assert_eq!(cfg.max_compress_input_tokens, Some(1024));
    }
}
