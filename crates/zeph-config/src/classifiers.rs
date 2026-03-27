// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

fn default_classifier_timeout_ms() -> u64 {
    5000
}

fn default_injection_model() -> String {
    "protectai/deberta-v3-small-prompt-injection-v2".into()
}

fn default_injection_threshold() -> f32 {
    0.8
}

fn default_ner_model() -> String {
    "iiiorg/piiranha-v1-detect-personal-information".into()
}

/// Configuration for the ML-backed classifier subsystem.
///
/// Placed under `[classifiers]` in `config.toml`. All fields are optional with safe defaults
/// so existing configs continue to work when this section is absent.
///
/// When `enabled = false` (the default), all classifier code is bypassed and the existing
/// regex-based detection runs unchanged.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ClassifiersConfig {
    /// Master switch. When `false`, classifiers are never loaded or invoked.
    #[serde(default)]
    pub enabled: bool,

    /// Per-inference timeout in milliseconds.
    ///
    /// On timeout the call site falls back to regex. Separate from model download time.
    #[serde(default = "default_classifier_timeout_ms")]
    pub timeout_ms: u64,

    /// `HuggingFace` repo ID for the injection detection model.
    #[serde(default = "default_injection_model")]
    pub injection_model: String,

    /// Minimum classifier score to treat a result as an injection.
    ///
    /// Range: `(0.0, 1.0]`. Conservative default of `0.8` minimises false positives.
    #[serde(default = "default_injection_threshold")]
    pub injection_threshold: f32,

    /// `HuggingFace` repo ID for the NER model used by `CandleNerClassifier`.
    ///
    /// Default: `iiiorg/piiranha-v1-detect-personal-information`.
    #[serde(default = "default_ner_model")]
    pub ner_model: String,
}

impl Default for ClassifiersConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_ms: default_classifier_timeout_ms(),
            injection_model: default_injection_model(),
            injection_threshold: default_injection_threshold(),
            ner_model: default_ner_model(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values() {
        let cfg = ClassifiersConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.timeout_ms, 5000);
        assert_eq!(
            cfg.injection_model,
            "protectai/deberta-v3-small-prompt-injection-v2"
        );
        assert!((cfg.injection_threshold - 0.8).abs() < 1e-6);
    }

    #[test]
    fn deserialize_empty_section_uses_defaults() {
        let cfg: ClassifiersConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.timeout_ms, 5000);
        assert_eq!(
            cfg.injection_model,
            "protectai/deberta-v3-small-prompt-injection-v2"
        );
        assert!((cfg.injection_threshold - 0.8).abs() < 1e-6);
    }

    #[test]
    fn deserialize_custom_values() {
        let toml = r#"
            enabled = true
            timeout_ms = 2000
            injection_model = "custom/model-v1"
            injection_threshold = 0.9
        "#;
        let cfg: ClassifiersConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.timeout_ms, 2000);
        assert_eq!(cfg.injection_model, "custom/model-v1");
        assert!((cfg.injection_threshold - 0.9).abs() < 1e-6);
    }

    #[test]
    fn serialize_roundtrip() {
        let original = ClassifiersConfig {
            enabled: true,
            timeout_ms: 3000,
            injection_model: "org/model".into(),
            injection_threshold: 0.75,
            ner_model: "org/ner-model".into(),
        };
        let serialized = toml::to_string(&original).unwrap();
        let deserialized: ClassifiersConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn partial_override_timeout_only() {
        let toml = "timeout_ms = 1000";
        let cfg: ClassifiersConfig = toml::from_str(toml).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.timeout_ms, 1000);
        assert_eq!(
            cfg.injection_model,
            "protectai/deberta-v3-small-prompt-injection-v2"
        );
        assert!((cfg.injection_threshold - 0.8).abs() < 1e-6);
    }
}
