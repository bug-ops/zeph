// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Write-time memory-consent gate configuration (issue #6490, `MemGhost`).
//!
//! See [`crate::memory::root::MemoryConfig::consent_gate`].

use serde::{Deserialize, Serialize};

fn default_confirm_threshold() -> String {
    "external_untrusted".to_owned()
}

fn default_disclose_threshold() -> String {
    "local_untrusted".to_owned()
}

/// Configuration for the write-time memory-consent gate, nested under `[memory.consent_gate]`
/// in TOML (issue #6490).
///
/// Gates memory writes derived from untrusted content (tool output, web scrapes, MCP
/// responses) behind either an interactive confirmation (`memory_save` tool path) or a
/// visible in-turn disclosure note (autonomous background tool-output writes, which must
/// never block on `Channel::confirm` per the non-blocking contract — see spec-039).
///
/// `confirm_threshold`/`disclose_threshold` accept the `snake_case` serialization of
/// `zeph_sanitizer::ContentTrustLevel` (`"trusted"`, `"local_untrusted"`,
/// `"external_untrusted"`). `zeph-config` cannot depend on `zeph-sanitizer` (the dependency
/// runs the other way), so these are plain strings parsed by callers via
/// `ContentTrustLevel::from_str_opt`.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.consent_gate]
/// enabled = true
/// confirm_threshold = "external_untrusted"
/// disclose_threshold = "local_untrusted"
/// audit_all = true
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ConsentGateConfig {
    /// Master switch. Default: `true`.
    pub enabled: bool,
    /// Minimum trust tier (inclusive) that requires interactive confirmation via
    /// `Channel::confirm` on the `memory_save` tool path. Default: `"external_untrusted"`.
    #[serde(default = "default_confirm_threshold")]
    pub confirm_threshold: String,
    /// Minimum trust tier (inclusive) that requires a visible in-turn disclosure note on
    /// autonomous background tool-output memory writes. Default: `"local_untrusted"`.
    #[serde(default = "default_disclose_threshold")]
    pub disclose_threshold: String,
    /// When `true`, every memory write is recorded in the audit log with source
    /// attribution, regardless of trust tier. Default: `true`.
    pub audit_all: bool,
}

impl Default for ConsentGateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            confirm_threshold: default_confirm_threshold(),
            disclose_threshold: default_disclose_threshold(),
            audit_all: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let cfg = ConsentGateConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.confirm_threshold, "external_untrusted");
        assert_eq!(cfg.disclose_threshold, "local_untrusted");
        assert!(cfg.audit_all);
    }

    #[test]
    fn deserializes_from_empty_table() {
        let cfg: ConsentGateConfig = toml::from_str("").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.confirm_threshold, "external_untrusted");
        assert_eq!(cfg.disclose_threshold, "local_untrusted");
    }

    #[test]
    fn deserializes_explicit_values() {
        let cfg: ConsentGateConfig = toml::from_str(
            "enabled = false\nconfirm_threshold = \"local_untrusted\"\naudit_all = false\n",
        )
        .unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.confirm_threshold, "local_untrusted");
        assert!(!cfg.audit_all);
    }
}
