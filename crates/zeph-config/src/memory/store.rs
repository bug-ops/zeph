// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Cross-thread key-value store configuration (spec-080, #6363).
//!
//! `LangGraph` `Store` parity — see [`crate::memory::root::MemoryConfig::store`] and
//! `crates/zeph-memory/src/store/cross_thread.rs`.

use serde::{Deserialize, Serialize};

fn default_max_value_bytes() -> usize {
    65536
}

/// Configuration for the generic namespaced cross-thread key-value store, nested under
/// `[memory.store]` in TOML (spec-080, #6363).
///
/// Disabled by default (FR-A-001): when `enabled = false`, no store read/write path is
/// exposed to orchestration or the CLI — zero behavior change.
///
/// # Example (TOML)
///
/// ```toml
/// [memory.store]
/// enabled = false
/// max_value_bytes = 65536
/// # search_provider = "fast"   # reserved for future semantic search
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CrossThreadStoreConfig {
    /// Master switch. Default: `false` (FR-A-001).
    pub enabled: bool,
    /// Maximum `value` size in bytes accepted by `store_put`; larger writes are rejected
    /// with a descriptive error rather than truncated (FR-A-005). Default: `65536`.
    #[serde(default = "default_max_value_bytes")]
    pub max_value_bytes: usize,
    /// Reserved for a future semantic-search extension over store values (`[memory.store]
    /// search_provider`, declare-once `*_provider` naming per `CLAUDE.md` §Multi-Model
    /// Design). Unused in v1 — MVP `store_search` is namespace-prefix + keyword match only
    /// (spec-080 §1 Out of Scope).
    #[serde(default)]
    pub search_provider: Option<String>,
}

impl Default for CrossThreadStoreConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_value_bytes: default_max_value_bytes(),
            search_provider: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let cfg = CrossThreadStoreConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_value_bytes, 65536);
        assert!(cfg.search_provider.is_none());
    }

    #[test]
    fn deserializes_from_empty_table() {
        let cfg: CrossThreadStoreConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_value_bytes, 65536);
    }

    #[test]
    fn deserializes_explicit_values() {
        let cfg: CrossThreadStoreConfig =
            toml::from_str("enabled = true\nmax_value_bytes = 1024\n").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.max_value_bytes, 1024);
    }
}
