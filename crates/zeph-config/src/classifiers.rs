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

fn default_injection_threshold_soft() -> f32 {
    0.5
}

fn default_pii_model() -> String {
    "iiiorg/piiranha-v1-detect-personal-information".into()
}

fn default_pii_threshold() -> f32 {
    0.75
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

    /// Resolved `HuggingFace` Hub API token.
    ///
    /// Must be the **token value** (not a vault key name) — resolved by the caller before
    /// constructing `ClassifiersConfig`. When `None`, model downloads are unauthenticated,
    /// which fails for gated or private repos.
    #[serde(default)]
    pub hf_token: Option<String>,

    /// When `true`, the ML injection classifier runs on direct user chat messages.
    ///
    /// Default `false`: the `DeBERTa` model is intended for external/untrusted content
    /// (tool output, web scrapes) — not for direct user input. Enabling this may cause
    /// false positives on benign conversational messages.
    #[serde(default)]
    pub scan_user_input: bool,

    /// `HuggingFace` repo ID for the injection detection model.
    #[serde(default = "default_injection_model")]
    pub injection_model: String,

    /// Soft threshold: classifier score at or above this emits a WARN log and increments
    /// the suspicious-injection metric, but content is allowed through.
    ///
    /// Range: `(0.0, 1.0]`. Default `0.5`. Must be ≤ `injection_threshold`.
    #[serde(default = "default_injection_threshold_soft")]
    pub injection_threshold_soft: f32,

    /// Hard threshold: classifier score at or above this blocks the content.
    ///
    /// Range: `(0.0, 1.0]`. Conservative default of `0.8` minimises false positives.
    /// Real-world ML injection classifiers have 12–37% recall gaps at high thresholds —
    /// defense-in-depth via regex fallback and spotlighting is mandatory.
    #[serde(default = "default_injection_threshold")]
    pub injection_threshold: f32,

    /// Optional SHA-256 hex digest of the injection model safetensors file.
    ///
    /// When set, the file is verified before loading. Mismatch aborts startup with an error.
    /// Useful for security-sensitive deployments to detect corruption or tampering.
    #[serde(default)]
    pub injection_model_sha256: Option<String>,

    /// Enable PII detection via the NER model (`pii_model`).
    ///
    /// When `true`, `CandlePiiClassifier` runs on user messages in addition to the
    /// regex-based `PiiFilter`. Both results are merged (union with deduplication).
    #[serde(default)]
    pub pii_enabled: bool,

    /// `HuggingFace` repo ID for the PII NER model.
    #[serde(default = "default_pii_model")]
    pub pii_model: String,

    /// Minimum per-token confidence to accept a PII label.
    ///
    /// Tokens below this threshold are treated as O (no entity).
    /// Default `0.75` balances recall on rarer entity types (DRIVERLICENSE, PASSPORT, IBAN)
    /// with precision. Raise to `0.85` to prefer precision over recall.
    #[serde(default = "default_pii_threshold")]
    pub pii_threshold: f32,

    /// Optional SHA-256 hex digest of the PII model safetensors file.
    #[serde(default)]
    pub pii_model_sha256: Option<String>,
}

impl Default for ClassifiersConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_ms: default_classifier_timeout_ms(),
            hf_token: None,
            scan_user_input: false,
            injection_model: default_injection_model(),
            injection_threshold_soft: default_injection_threshold_soft(),
            injection_threshold: default_injection_threshold(),
            injection_model_sha256: None,
            pii_enabled: false,
            pii_model: default_pii_model(),
            pii_threshold: default_pii_threshold(),
            pii_model_sha256: None,
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
        assert!(cfg.hf_token.is_none());
        assert!(!cfg.scan_user_input);
        assert_eq!(
            cfg.injection_model,
            "protectai/deberta-v3-small-prompt-injection-v2"
        );
        assert!((cfg.injection_threshold_soft - 0.5).abs() < 1e-6);
        assert!((cfg.injection_threshold - 0.8).abs() < 1e-6);
        assert!(cfg.injection_model_sha256.is_none());
        assert!(!cfg.pii_enabled);
        assert_eq!(
            cfg.pii_model,
            "iiiorg/piiranha-v1-detect-personal-information"
        );
        assert!((cfg.pii_threshold - 0.75).abs() < 1e-6);
        assert!(cfg.pii_model_sha256.is_none());
    }

    #[test]
    fn hf_token_and_scan_user_input_round_trip() {
        let toml = r#"
            hf_token = "hf_secret"
            scan_user_input = true
        "#;
        let cfg: ClassifiersConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.hf_token.as_deref(), Some("hf_secret"));
        assert!(cfg.scan_user_input);
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
        assert!((cfg.injection_threshold_soft - 0.5).abs() < 1e-6);
        assert!((cfg.injection_threshold - 0.8).abs() < 1e-6);
        assert!(!cfg.pii_enabled);
        assert!((cfg.pii_threshold - 0.75).abs() < 1e-6);
    }

    #[test]
    fn deserialize_custom_values() {
        let toml = r#"
            enabled = true
            timeout_ms = 2000
            injection_model = "custom/model-v1"
            injection_threshold = 0.9
            pii_enabled = true
            pii_threshold = 0.85
        "#;
        let cfg: ClassifiersConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.timeout_ms, 2000);
        assert_eq!(cfg.injection_model, "custom/model-v1");
        assert!((cfg.injection_threshold_soft - 0.5).abs() < 1e-6);
        assert!((cfg.injection_threshold - 0.9).abs() < 1e-6);
        assert!(cfg.pii_enabled);
        assert!((cfg.pii_threshold - 0.85).abs() < 1e-6);
    }

    #[test]
    fn deserialize_sha256_fields() {
        let toml = r#"
            injection_model_sha256 = "abc123"
            pii_model_sha256 = "def456"
        "#;
        let cfg: ClassifiersConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.injection_model_sha256.as_deref(), Some("abc123"));
        assert_eq!(cfg.pii_model_sha256.as_deref(), Some("def456"));
    }

    #[test]
    fn serialize_roundtrip() {
        let original = ClassifiersConfig {
            enabled: true,
            timeout_ms: 3000,
            hf_token: Some("hf_test_token".into()),
            scan_user_input: true,
            injection_model: "org/model".into(),
            injection_threshold_soft: 0.45,
            injection_threshold: 0.75,
            injection_model_sha256: Some("deadbeef".into()),
            pii_enabled: true,
            pii_model: "org/pii-model".into(),
            pii_threshold: 0.80,
            pii_model_sha256: None,
        };
        let serialized = toml::to_string(&original).unwrap();
        let deserialized: ClassifiersConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn dual_threshold_deserialization() {
        let toml = r#"
            injection_threshold_soft = 0.4
            injection_threshold = 0.85
        "#;
        let cfg: ClassifiersConfig = toml::from_str(toml).unwrap();
        assert!((cfg.injection_threshold_soft - 0.4).abs() < 1e-6);
        assert!((cfg.injection_threshold - 0.85).abs() < 1e-6);
    }

    #[test]
    fn soft_threshold_defaults_when_only_hard_provided() {
        let toml = "injection_threshold = 0.9";
        let cfg: ClassifiersConfig = toml::from_str(toml).unwrap();
        assert!((cfg.injection_threshold_soft - 0.5).abs() < 1e-6);
        assert!((cfg.injection_threshold - 0.9).abs() < 1e-6);
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
        assert!((cfg.injection_threshold_soft - 0.5).abs() < 1e-6);
        assert!((cfg.injection_threshold - 0.8).abs() < 1e-6);
    }
}
