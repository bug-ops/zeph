// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `[serve]` config: settings for the `zeph serve` persistent agent service (spec-068 §9).
//!
//! `zeph serve` runs a long-lived process exposing sessions over an HTTP/SSE API and (optionally)
//! ACP, backed by the same durable JSONL event log as every other channel. This module only
//! declares the config surface — see `crates/zeph-core` for `SessionActor`/`LiveSessionRegistry`
//! and `src/serve/` for the HTTP handlers.

use serde::{Deserialize, Serialize};

/// Top-level `[serve]` config block (spec-068 §9).
///
/// # Example (TOML)
///
/// ```toml
/// [serve]
/// http_addr = "127.0.0.1:8420"
/// require_auth = true
/// auth_token_vault_key = "ZEPH_SERVE_AUTH_TOKEN"
/// max_sessions = 50
/// session_idle_ttl_secs = 1800
/// max_queued_prompts = 8
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ServeConfig {
    /// Address the HTTP/SSE API binds to. Default: `"127.0.0.1:8420"`.
    pub http_addr: String,
    /// Require a bearer token on all `/sessions*` endpoints (`/health` is always
    /// unauthenticated). Default: `true`.
    ///
    /// Per CLAUDE.md's vault-only secrets policy, the token itself is never stored in this
    /// config file — only the vault key name to resolve it from (`auth_token_vault_key`).
    pub require_auth: bool,
    /// Zeph age-vault key name to resolve the bearer token from at startup (spec-068 NFR-S4).
    /// Default: `"ZEPH_SERVE_AUTH_TOKEN"`.
    pub auth_token_vault_key: String,
    /// Maximum number of concurrent live sessions the `LiveSessionRegistry` will hold.
    /// Default: `50`.
    pub max_sessions: usize,
    /// Seconds of no attached broadcast receivers before `serve.evict` shuts down and removes a
    /// session's `SessionActor` (spec-068 §9.3). Default: `1800` (30 minutes).
    pub session_idle_ttl_secs: u64,
    /// Bounded mpsc capacity for a `SessionActor`'s prompt mailbox; a full mailbox returns HTTP
    /// 429 to the caller rather than blocking (spec-068 §9.2). Default: `8`.
    pub max_queued_prompts: usize,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            http_addr: "127.0.0.1:8420".to_owned(),
            require_auth: true,
            auth_token_vault_key: "ZEPH_SERVE_AUTH_TOKEN".to_owned(),
            max_sessions: 50,
            session_idle_ttl_secs: 1800,
            max_queued_prompts: 8,
        }
    }
}
