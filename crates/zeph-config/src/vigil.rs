// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! VIGIL (Verify-Before-Commit Intent Gate) configuration.
//!
//! `VigilConfig` is nested under `[security.vigil]` in TOML. It controls the pre-sanitizer
//! regex tripwire that checks tool outputs against injection patterns before they enter LLM
//! context. See spec `010-6-vigil-intent-anchoring` for the full threat model.
//!
//! **VIGIL v1 is a best-effort regex tripwire, not an injection-resistance claim.** The
//! canonical defense remains `ContentSanitizer` + spotlighting. VIGIL adds an explicit
//! block/sanitize action and a correlated audit trail.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

fn default_vigil_enabled() -> bool {
    true
}

fn default_strict_mode() -> bool {
    false
}

fn default_sanitize_max_chars() -> usize {
    2048
}

fn default_exempt_tools() -> Vec<String> {
    vec![
        "memory_search".into(),
        "read_overflow".into(),
        "load_skill".into(),
        "invoke_skill".into(),
        "schedule_deferred".into(),
    ]
}

/// VIGIL verify-before-commit configuration, nested under `[security.vigil]` in TOML.
///
/// Controls the pre-sanitizer regex gate that inspects tool outputs for injection patterns
/// before they are committed to the LLM context. VIGIL runs *before* `ContentSanitizer`.
///
/// # VIGIL v1 threat model
///
/// VIGIL v1 is a best-effort regex tripwire that catches low-effort textbook injections.
/// It is NOT a claim of injection resistance. The existing `ContentSanitizer` + spotlighting
/// pipeline remains the defense-in-depth primary layer.
///
/// Explicit non-goals for v1:
/// - Unicode homoglyphs, zero-width joiners, base64/rot13 encodings.
/// - HTML-entity / URL-percent encoding inside embedded content.
/// - Non-English pattern matching (the regex bank is English-only).
/// - Paraphrase / soft injection (requires semantic grounding — v2 scope).
///
/// # Example (TOML)
///
/// ```toml
/// [security.vigil]
/// enabled = true
/// strict_mode = false
/// sanitize_max_chars = 2048
/// extra_patterns = []
/// exempt_tools = ["memory_search", "read_overflow", "load_skill", "schedule_deferred"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct VigilConfig {
    /// Master switch. When `false`, VIGIL is bypassed entirely. Default: `true`.
    #[serde(default = "default_vigil_enabled")]
    pub enabled: bool,
    /// When `true`, flagged outputs are replaced by the security sentinel (Block action).
    /// When `false` (default), outputs are truncated to `sanitize_max_chars` and annotated
    /// (Sanitize action), then passed to `sanitize_tool_output`.
    #[serde(default = "default_strict_mode")]
    pub strict_mode: bool,
    /// Truncation budget for the Sanitize action. Default: `2048`.
    #[serde(default = "default_sanitize_max_chars")]
    pub sanitize_max_chars: usize,
    /// Operator-supplied additional injection patterns.
    ///
    /// Validated at config load: each entry must compile with `regex::Regex::new`,
    /// be at most 1024 characters long, and the collection must have at most 64 entries.
    /// Invalid patterns cause config load to fail (no silent skip).
    #[serde(default)]
    pub extra_patterns: Vec<String>,
    /// Tool identifiers exempt from VIGIL. Exempting a tool is a trust delegation —
    /// only exempt tools whose outputs are validated by the orchestrator or read from
    /// trusted internal stores. Default: `["memory_search", "read_overflow", "load_skill",
    /// "schedule_deferred"]`.
    #[serde(default = "default_exempt_tools")]
    pub exempt_tools: Vec<String>,
}

impl Default for VigilConfig {
    fn default() -> Self {
        Self {
            enabled: default_vigil_enabled(),
            strict_mode: default_strict_mode(),
            sanitize_max_chars: default_sanitize_max_chars(),
            extra_patterns: Vec::new(),
            exempt_tools: default_exempt_tools(),
        }
    }
}

impl VigilConfig {
    /// Validate `extra_patterns`: each must compile, be ≤1024 chars, and total count ≤64.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when any pattern is invalid, too long, or the
    /// collection exceeds the 64-entry cap.
    pub fn validate(&self) -> Result<(), ConfigError> {
        const MAX_PATTERN_LEN: usize = 1024;
        const MAX_PATTERN_COUNT: usize = 64;

        if self.extra_patterns.len() > MAX_PATTERN_COUNT {
            return Err(ConfigError::Validation(format!(
                "security.vigil.extra_patterns: {} entries exceed the cap of {}",
                self.extra_patterns.len(),
                MAX_PATTERN_COUNT,
            )));
        }

        for (idx, pat) in self.extra_patterns.iter().enumerate() {
            if pat.len() > MAX_PATTERN_LEN {
                return Err(ConfigError::Validation(format!(
                    "security.vigil.extra_patterns[{idx}]: pattern length {} exceeds cap of {MAX_PATTERN_LEN}",
                    pat.len(),
                )));
            }
            regex::RegexBuilder::new(pat)
                .size_limit(1 << 20)
                .dfa_size_limit(1 << 20)
                .build()
                .map_err(|e| {
                    ConfigError::Validation(format!(
                        "security.vigil.extra_patterns[{idx}]: invalid regex: {e}"
                    ))
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_expected_values() {
        let cfg = VigilConfig::default();
        assert!(cfg.enabled);
        assert!(!cfg.strict_mode);
        assert_eq!(cfg.sanitize_max_chars, 2048);
        assert!(cfg.extra_patterns.is_empty());
        assert!(cfg.exempt_tools.contains(&"memory_search".to_owned()));
        assert!(cfg.exempt_tools.contains(&"load_skill".to_owned()));
        assert!(cfg.exempt_tools.contains(&"invoke_skill".to_owned()));
    }

    #[test]
    fn validate_empty_extra_patterns_ok() {
        let cfg = VigilConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_valid_extra_pattern_ok() {
        let cfg = VigilConfig {
            extra_patterns: vec!["ignore.*previous".into()],
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_invalid_regex_fails() {
        let cfg = VigilConfig {
            extra_patterns: vec!["[".into()],
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }

    #[test]
    fn validate_too_many_patterns_fails() {
        let cfg = VigilConfig {
            extra_patterns: (0..65).map(|i| format!("pattern{i}")).collect(),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("exceed the cap"));
    }

    #[test]
    fn validate_pattern_too_long_fails() {
        let long = "a".repeat(1025);
        let cfg = VigilConfig {
            extra_patterns: vec![long],
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("length"));
    }

    #[test]
    fn serde_roundtrip() {
        let cfg = VigilConfig {
            enabled: false,
            strict_mode: true,
            sanitize_max_chars: 512,
            extra_patterns: vec!["test".into()],
            exempt_tools: vec!["shell".into()],
        };
        let toml = toml::to_string(&cfg).expect("serialize");
        let back: VigilConfig = toml::from_str(&toml).expect("deserialize");
        assert!(!back.enabled);
        assert!(back.strict_mode);
        assert_eq!(back.sanitize_max_chars, 512);
        assert_eq!(back.extra_patterns, vec!["test"]);
    }
}
