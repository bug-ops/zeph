// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-data configuration for vault-anchor downgrade-resistance (`[integrity]`, issue #6449).
//!
//! Layers on top of the transcript/session hash-chain feature (issue #6360): the chain alone
//! detects in-place edits and a partial strip of chain metadata, but not a fully consistent
//! whole-file strip. `[integrity]` controls whether a per-file vault anchor is written on
//! finalize/close to close that gap. See `zeph_common::anchor` for the mechanism.

use serde::{Deserialize, Serialize};

/// Vault-anchor downgrade-resistance posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnchorMode {
    /// Write and check a per-file vault anchor (the default, whenever the age vault + a
    /// history-integrity key are available). Degrades gracefully — never a hard failure — to
    /// chain-only (#6453-level) protection if the vault isn't reachable at bootstrap; see
    /// `zeph_core::anchor_store`'s startup warning.
    #[default]
    Vault,
    /// Explicit opt-out: transcripts/sessions stay chain-verified (#6453) but not
    /// downgrade-resistant against a whole-file strip.
    None,
}

/// Configuration for vault-anchor downgrade-resistance (`[integrity]`, issue #6449).
///
/// # Examples
///
/// ```
/// use zeph_config::{AnchorMode, IntegrityConfig};
///
/// let cfg: IntegrityConfig = toml::from_str("").unwrap();
/// assert_eq!(cfg.anchor, AnchorMode::Vault);
/// assert_eq!(cfg.max_session_anchors, 512);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrityConfig {
    /// Vault-anchor downgrade-resistance posture.
    pub anchor: AnchorMode,
    /// Upper bound on the number of session anchors retained in the vault at once. Once
    /// exceeded, the reconcile-and-cap sweep evicts the oldest anchors (ordered by the
    /// vault-embedded `written_at` field, never filesystem mtime) down to this cap — those
    /// sessions degrade to chain-only (#6453-level) protection, never a brick (an evicted
    /// session still opens normally). Transcript anchors are independently bounded by
    /// `subagent.transcript_max_files` and need no separate cap. Default `512`: typical
    /// single-user/small-team deployments never evict.
    pub max_session_anchors: usize,
}

impl Default for IntegrityConfig {
    fn default() -> Self {
        Self {
            anchor: AnchorMode::Vault,
            max_session_anchors: 512,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_yields_spec_defaults() {
        let cfg: IntegrityConfig = toml::from_str("").unwrap();
        assert_eq!(cfg, IntegrityConfig::default());
    }

    #[test]
    fn anchor_none_deserializes() {
        let cfg: IntegrityConfig = toml::from_str("anchor = \"none\"").unwrap();
        assert_eq!(cfg.anchor, AnchorMode::None);
    }

    #[test]
    fn max_session_anchors_is_overridable() {
        let cfg: IntegrityConfig = toml::from_str("max_session_anchors = 10").unwrap();
        assert_eq!(cfg.max_session_anchors, 10);
        assert_eq!(
            cfg.anchor,
            AnchorMode::Vault,
            "unspecified fields keep their default"
        );
    }
}
