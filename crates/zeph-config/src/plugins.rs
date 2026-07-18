// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Plugin subsystem configuration (`[plugins]`).

use serde::{Deserialize, Serialize};

fn default_reputation_enabled() -> bool {
    true
}

fn default_reputation_similarity_threshold() -> f32 {
    0.65
}

fn default_reputation_min_name_len() -> usize {
    3
}

/// Top-level plugin subsystem configuration (`[plugins]`).
///
/// Currently holds only the install-time reputation (typosquat) check (spec-043, #5864); more
/// plugin-wide settings may be added here later.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PluginsConfig {
    /// Install-time name-similarity typosquat check (`[plugins.reputation]`).
    #[serde(default)]
    pub reputation: ReputationConfig,
}

/// Install-time plugin/skill name-similarity ("typosquat") advisory check (spec-043, #5864).
///
/// Compares an incoming plugin's declared name and skill names against
/// `zeph_skills::bundled::bundled_skill_names()` plus managed and other installed plugins'
/// skill names, using a Levenshtein-based similarity ratio computed entirely locally — zero
/// network calls (NFR-001). Advisory by default (`enforcement = "warn"`, FR-006/SC-004).
///
/// # Examples
///
/// ```toml
/// [plugins.reputation]
/// enabled = true
/// similarity_threshold = 0.65
/// min_name_len = 3
/// enforcement = "warn"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReputationConfig {
    /// Enable the check at install time. Default: `true` (advisory, zero-network, mirrors the
    /// on-by-default posture of the existing skill-body injection scan).
    #[serde(default = "default_reputation_enabled")]
    pub enabled: bool,
    /// Similarity ratio in `[0, 1]` at or above which a near-match warns. Higher = stricter
    /// (requires a closer match) = fewer warnings. Default `0.65` — the loosest value that
    /// still catches the motivating `github-pr`/`git-pr` example (similarity 0.667) with a
    /// margin, while producing zero false positives among Zeph's own bundled skill names.
    #[serde(default = "default_reputation_similarity_threshold")]
    pub similarity_threshold: f32,
    /// Skip comparisons where the shorter of the two compared names has fewer than this many
    /// characters. Default `3` — covers the bundled `git` skill (spec-043 M1); 1-2 character
    /// names are always skipped as noise regardless of this setting.
    #[serde(default = "default_reputation_min_name_len")]
    pub min_name_len: usize,
    /// `"warn"` (default, advisory-only — install/update proceeds) or `"block"` (opt-in hard
    /// gate: the install/update is refused before any file is written or swapped).
    /// `zeph plugin add --strict-reputation` overrides this to `"block"` for a single
    /// invocation without changing the persisted config.
    #[serde(default)]
    pub enforcement: ReputationEnforcement,
}

impl Default for ReputationConfig {
    fn default() -> Self {
        Self {
            enabled: default_reputation_enabled(),
            similarity_threshold: default_reputation_similarity_threshold(),
            min_name_len: default_reputation_min_name_len(),
            enforcement: ReputationEnforcement::default(),
        }
    }
}

/// Enforcement posture for [`ReputationConfig`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReputationEnforcement {
    /// Surface a warning; the install/update proceeds (FR-006, SC-004 default posture).
    #[default]
    Warn,
    /// Refuse the install/update before any file is written or swapped (opt-in, FR-006).
    Block,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_config_default_matches_documented_values() {
        let cfg = PluginsConfig::default();
        assert!(cfg.reputation.enabled);
        assert!((cfg.reputation.similarity_threshold - 0.65).abs() < f32::EPSILON);
        assert_eq!(cfg.reputation.min_name_len, 3);
        assert_eq!(cfg.reputation.enforcement, ReputationEnforcement::Warn);
    }

    #[test]
    fn reputation_config_deserializes_from_partial_toml() {
        let toml_str = "enabled = false\n";
        let cfg: ReputationConfig = toml::from_str(toml_str).unwrap();
        assert!(!cfg.enabled);
        // Fields absent from the input fall back to defaults, not zero values.
        assert!((cfg.similarity_threshold - 0.65).abs() < f32::EPSILON);
        assert_eq!(cfg.min_name_len, 3);
    }

    #[test]
    fn reputation_enforcement_serializes_snake_case() {
        let cfg = ReputationConfig {
            enforcement: ReputationEnforcement::Block,
            ..ReputationConfig::default()
        };
        let toml_str = toml::to_string(&cfg).unwrap();
        assert!(
            toml_str.contains("enforcement = \"block\""),
            "got: {toml_str}"
        );
    }
}
