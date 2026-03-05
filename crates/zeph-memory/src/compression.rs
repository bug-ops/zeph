// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

/// Controls when and how context compression is applied.
///
/// `Reactive` (default): compression only triggers when the context window is nearly full.
/// `Proactive`: compression is triggered earlier, at a configurable token threshold, using a
/// dedicated model to keep the context well below the hard limit.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum CompressionStrategy {
    /// Compression triggers reactively when the context exceeds the compaction threshold.
    /// This is the default and preserves existing behaviour.
    #[default]
    Reactive,
    /// Compression triggers proactively when `current_tokens > threshold_tokens`.
    Proactive {
        /// Token count at which proactive compression is triggered.
        threshold_tokens: usize,
        /// Maximum tokens the LLM may use for the summary output.
        max_summary_tokens: usize,
    },
}

impl CompressionStrategy {
    #[must_use]
    pub fn is_proactive(&self) -> bool {
        matches!(self, Self::Proactive { .. })
    }

    #[must_use]
    pub fn threshold_tokens(&self) -> Option<usize> {
        match self {
            Self::Proactive {
                threshold_tokens, ..
            } => Some(*threshold_tokens),
            Self::Reactive => None,
        }
    }

    #[must_use]
    pub fn max_summary_tokens(&self) -> Option<usize> {
        match self {
            Self::Proactive {
                max_summary_tokens, ..
            } => Some(*max_summary_tokens),
            Self::Reactive => None,
        }
    }
}

/// Configuration for the `[memory.compression]` TOML section.
fn default_compression_min_messages() -> usize {
    4
}

/// Configuration for the `[memory.compression]` TOML section.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompressionConfig {
    #[serde(flatten)]
    pub strategy: CompressionStrategy,
    /// Explicit model identifier used for compression LLM calls.
    ///
    /// Required when `strategy = "proactive"`. Omitting this field while using proactive
    /// strategy is a startup validation error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Minimum number of messages required before proactive compression is attempted.
    ///
    /// Avoids triggering compression when the context is nearly empty and compaction
    /// would immediately return early. Defaults to `4`.
    #[serde(default = "default_compression_min_messages")]
    pub min_messages: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            strategy: CompressionStrategy::default(),
            model: None,
            min_messages: default_compression_min_messages(),
        }
    }
}

impl CompressionConfig {
    /// Validate the compression configuration.
    ///
    /// # Errors
    ///
    /// Returns an error string when:
    /// - `strategy = "proactive"` and `model` is not set
    /// - `threshold_tokens = 0`
    /// - `max_summary_tokens = 0`
    /// - `max_summary_tokens >= threshold_tokens`
    pub fn validate(&self) -> Result<(), String> {
        match &self.strategy {
            CompressionStrategy::Proactive {
                threshold_tokens,
                max_summary_tokens,
            } => {
                if self.model.is_none() {
                    return Err(
                        "compression strategy is \"proactive\" but no model is set; \
                         set [memory.compression] model = \"<model-id>\""
                            .to_string(),
                    );
                }
                if *threshold_tokens == 0 {
                    return Err("compression.threshold_tokens must be greater than 0".to_string());
                }
                if *max_summary_tokens == 0 {
                    return Err("compression.max_summary_tokens must be greater than 0".to_string());
                }
                if *max_summary_tokens >= *threshold_tokens {
                    return Err(format!(
                        "compression.max_summary_tokens ({max_summary_tokens}) must be less \
                         than threshold_tokens ({threshold_tokens})"
                    ));
                }
                Ok(())
            }
            CompressionStrategy::Reactive => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_strategy_is_reactive() {
        let cfg = CompressionConfig::default();
        assert!(!cfg.strategy.is_proactive());
        assert!(cfg.strategy.threshold_tokens().is_none());
        assert!(cfg.strategy.max_summary_tokens().is_none());
    }

    #[test]
    fn reactive_deserializes_from_json() {
        let s = r#"{"strategy":"reactive"}"#;
        let cfg: CompressionConfig = serde_json::from_str(s).expect("parse");
        assert!(!cfg.strategy.is_proactive());
    }

    #[test]
    fn proactive_deserializes_from_json() {
        let s = r#"{"strategy":"proactive","threshold_tokens":8000,"max_summary_tokens":2000,"model":"claude-haiku"}"#;
        let cfg: CompressionConfig = serde_json::from_str(s).expect("parse");
        assert!(cfg.strategy.is_proactive());
        assert_eq!(cfg.strategy.threshold_tokens(), Some(8000));
        assert_eq!(cfg.strategy.max_summary_tokens(), Some(2000));
        assert_eq!(cfg.model.as_deref(), Some("claude-haiku"));
    }

    #[test]
    fn validate_proactive_missing_model() {
        let cfg = CompressionConfig {
            strategy: CompressionStrategy::Proactive {
                threshold_tokens: 8000,
                max_summary_tokens: 2000,
            },
            model: None,
            min_messages: 4,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_proactive_zero_summary_tokens() {
        let cfg = CompressionConfig {
            strategy: CompressionStrategy::Proactive {
                threshold_tokens: 8000,
                max_summary_tokens: 0,
            },
            model: Some("m".into()),
            min_messages: 4,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_proactive_zero_threshold() {
        let cfg = CompressionConfig {
            strategy: CompressionStrategy::Proactive {
                threshold_tokens: 0,
                max_summary_tokens: 2000,
            },
            model: Some("model".into()),
            min_messages: 4,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_proactive_summary_gte_threshold() {
        let cfg = CompressionConfig {
            strategy: CompressionStrategy::Proactive {
                threshold_tokens: 1000,
                max_summary_tokens: 1000,
            },
            model: Some("model".into()),
            min_messages: 4,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_proactive_valid() {
        let cfg = CompressionConfig {
            strategy: CompressionStrategy::Proactive {
                threshold_tokens: 8000,
                max_summary_tokens: 2000,
            },
            model: Some("claude-haiku".into()),
            min_messages: 4,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_reactive_always_ok() {
        let cfg = CompressionConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn round_trip_serde() {
        let cfg = CompressionConfig {
            strategy: CompressionStrategy::Proactive {
                threshold_tokens: 8000,
                max_summary_tokens: 2000,
            },
            model: Some("claude-haiku".into()),
            min_messages: 4,
        };
        let s = serde_json::to_string(&cfg).expect("serialize");
        let back: CompressionConfig = serde_json::from_str(&s).expect("deserialize");
        assert!(back.strategy.is_proactive());
        assert_eq!(back.model.as_deref(), Some("claude-haiku"));
        assert_eq!(back.min_messages, 4);
    }

    #[test]
    fn min_messages_deserializes_from_json() {
        let s = r#"{"strategy":"reactive","min_messages":6}"#;
        let cfg: CompressionConfig = serde_json::from_str(s).expect("parse");
        assert_eq!(cfg.min_messages, 6);
    }

    #[test]
    fn min_messages_defaults_to_4() {
        let s = r#"{"strategy":"reactive"}"#;
        let cfg: CompressionConfig = serde_json::from_str(s).expect("parse");
        assert_eq!(cfg.min_messages, 4);
    }
}
