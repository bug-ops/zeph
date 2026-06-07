// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration types for the `zeph://` deep-link scheme (spec-066).
//!
//! This module provides [`DeepLinkConfig`] for the `[deep_link]` config section and the
//! [`AcpPreference`] enum for controlling ACP attach behaviour. All types use explicit
//! `Default` impls where needed to preserve secure defaults that differ from Rust's
//! type-level defaults (e.g., `bool::default()` is `false`, but `confirm_before_prompt`
//! must default to `true`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration for the `[deep_link]` section.
///
/// Controls security gates, path allowlists, and ACP attach preferences for sessions
/// initiated via `zeph://` URIs. Gated by the `deep-link` Cargo feature.
///
/// All fields have `#[serde(default)]` so existing configs without `[deep_link]` parse
/// cleanly with secure defaults.
///
/// # Examples
///
/// ```
/// use zeph_config::deep_link::DeepLinkConfig;
///
/// let cfg = DeepLinkConfig::default();
/// assert!(cfg.confirm_before_prompt, "default must require confirmation");
/// assert!(cfg.allowed_cwd_roots.is_empty());
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DeepLinkConfig {
    /// When `true`, the agent presents the injected prompt to the user and requires
    /// explicit confirmation before enqueuing it. Defaults to `true` (secure default).
    ///
    /// Set to `false` only in environments where prompt injection risk is fully
    /// mitigated (e.g., internal tooling with a known-good URL source).
    pub confirm_before_prompt: bool,

    /// Restrict the working directory to these root prefixes. An empty list means any
    /// non-denylisted absolute path is accepted.
    ///
    /// Validation is performed by `validate_deep_link_cwd` in `src/url_scheme/validate.rs`
    /// after canonicalization and denylist checks (INV-CWD order).
    pub allowed_cwd_roots: Vec<PathBuf>,

    /// ACP server attach preference for deep-link sessions.
    ///
    /// `Never` (default) always spawns a fresh session. `Auto` and `Always` are reserved
    /// for v2 and are currently treated identically to `Never`.
    pub prefer_acp: AcpPreference,
}

impl Default for DeepLinkConfig {
    /// Returns a `DeepLinkConfig` with secure defaults.
    ///
    /// `confirm_before_prompt` is `true` — this is intentional. `bool::default()` is
    /// `false`, so `#[derive(Default)]` would silently produce an insecure default.
    fn default() -> Self {
        Self {
            confirm_before_prompt: true,
            allowed_cwd_roots: Vec::new(),
            prefer_acp: AcpPreference::Never,
        }
    }
}

/// ACP server attach preference for deep-link-initiated sessions.
///
/// `Never` is the only supported mode in v1. `Auto` and `Always` are parsed and accepted
/// by the config but treated identically to `Never` until the v2 ACP discovery mechanism
/// is implemented (see spec-066 §13 for the v2 design sketch).
///
/// # Examples
///
/// ```
/// use zeph_config::deep_link::AcpPreference;
///
/// let pref = AcpPreference::default();
/// assert!(matches!(pref, AcpPreference::Never));
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AcpPreference {
    /// Always spawn a fresh session; never attempt ACP attach. This is the only
    /// mode that has effect in v1.
    #[default]
    Never,
    /// Attempt ACP attach when a running session is detected; fall back to spawn.
    /// Reserved for v2 — currently treated as `Never`.
    Auto,
    /// Always attempt ACP attach; fail if no running session is found.
    /// Reserved for v2 — currently treated as `Never`.
    Always,
}
