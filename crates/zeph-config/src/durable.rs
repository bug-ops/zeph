// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-data configuration for the durable execution layer (`[durable]`).
//!
//! These types mirror the `[durable]` TOML section and are the single source of truth for the
//! durable execution configuration. They live in `zeph-config` (alongside every other subsystem
//! config) so the aggregate [`Config`](crate::Config) can hold them without forcing the heavy
//! `zeph-db`/`sqlx` dependency tree of `zeph-durable` onto the config layer. The `zeph-durable`
//! crate re-exports these types and applies the AEAD enforcement policy (the `encryption_gate`)
//! on top of them.
//!
//! Every field carries a spec default via the container-level `#[serde(default)]` attribute backed
//! by [`Default`], so deserializing an empty table yields a fully-populated, spec-compliant
//! configuration. No credentials appear inline — the AEAD key and any Restate endpoints are
//! resolved from the vault by key name (spec-038 vault contract), never stored here.

use serde::{Deserialize, Serialize};

/// Which journal backend an execution uses.
///
/// `Restate` is only meaningful when the `restate` feature and an external Restate server are
/// available; the variant is accepted in configuration regardless so a config can be authored
/// ahead of the backend being compiled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DurableBackend {
    /// Dedicated `durable.db` SQLite/Postgres file managed in-process. The default.
    #[default]
    Local,
    /// External Restate server (feature-gated, server deployments only).
    Restate,
}

/// Configuration for the durable execution layer (`[durable]`).
///
/// # Examples
///
/// ```
/// use zeph_config::DurableConfig;
///
/// // An empty table deserializes to the spec defaults.
/// let cfg: DurableConfig = toml::from_str("").unwrap();
/// assert!(!cfg.enabled);
/// assert_eq!(cfg.journal_ack_timeout_ms, 5000);
/// assert_eq!(cfg.max_payload_bytes, 1_048_576);
/// ```
#[allow(clippy::struct_excessive_bools)] // config struct — boolean flags are idiomatic for TOML-deserialized configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DurableConfig {
    /// Master opt-in. When `false`, no journal is opened and behavior is identical to a build
    /// without the durable layer.
    pub enabled: bool,
    /// Selected journal backend.
    pub backend: DurableBackend,
    /// Encrypt payloads with AEAD. A `false` value is a development-only override (it emits a
    /// startup warning) and is forbidden for non-local backends (INV-8).
    pub encrypt_payload: bool,
    /// Operator-declared flag: the durable journal database is reachable by more than one
    /// process/client (a network-shared volume, or any future Postgres-backed durable
    /// deployment). Required `true` for such deployments — enforced by `encryption_gate`
    /// (INV-8), which forbids `encrypt_payload = false` when this is `true`. Default `false`
    /// (an ordinary single-user local deployment).
    pub shared_db: bool,
    /// P1 adapter: wrap agent-loop steps in durable steps.
    ///
    /// When `true` (with `enabled = true`), every ordinary agent turn's LLM call is journaled
    /// via a `DurableContext` opened lazily on the session's first turn (see
    /// `zeph_core::agent::Agent::ensure_session_durable_ctx`, #5452). The execution is keyed on
    /// the session's `ConversationId`, so this adapter requires semantic memory (`[memory]`) to
    /// be enabled — without it, `conversation_id` is never set and the agent degrades to
    /// non-durable with a one-time `tracing::warn!` at bootstrap, even though `agent_turns = true`
    /// looks fully enabled in the config.
    pub agent_turns: bool,
    /// P2 adapter: journal the orchestration `/plan resume` replan budget.
    pub orchestration: bool,
    /// P3 adapter: exactly-once scheduler job fire.
    pub scheduler: bool,
    /// P4 adapter: durable promise for subagent spawn/await.
    ///
    /// Requires `agent_turns = true` as well: the durable seat is only attached when the
    /// session's `DurableContext` (populated by the `agent_turns` adapter) is already `Some`
    /// (#5452).
    pub subagent: bool,
    /// Group-commit interval for buffered appends, in milliseconds.
    pub journal_flush_interval_ms: u64,
    /// Timeout for an acknowledged append before degrading to non-durable mode, in milliseconds.
    pub journal_ack_timeout_ms: u64,
    /// In-execution step cap (soft fold at 90%, hard abort at 100%).
    pub max_steps_per_execution: u32,
    /// Maximum payload size in bytes, enforced on both append and read.
    pub max_payload_bytes: u64,
    /// Database fallback poll interval for parked promises, in seconds.
    pub promise_poll_interval_secs: u64,
    /// Above this many parked promises, resolution falls back to pure polling.
    pub max_parked_promises: u32,
    /// Journal retention and compaction policy (`[durable.retention]`).
    pub retention: RetentionPolicy,
}

impl Default for DurableConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: DurableBackend::Local,
            encrypt_payload: true,
            shared_db: false,
            agent_turns: true,
            orchestration: true,
            scheduler: true,
            subagent: true,
            journal_flush_interval_ms: 10,
            journal_ack_timeout_ms: 5000,
            max_steps_per_execution: 10_000,
            max_payload_bytes: 1_048_576,
            promise_poll_interval_secs: 2,
            max_parked_promises: 1000,
            retention: RetentionPolicy::default(),
        }
    }
}

/// Journal retention and compaction policy (`[durable.retention]`).
///
/// Drives the background prune sweep, which never runs on the dispatch hot path.
///
/// # Examples
///
/// ```
/// use zeph_config::RetentionPolicy;
///
/// let policy = RetentionPolicy::default();
/// assert_eq!(policy.ttl_completed_secs, 604_800); // 7 days
/// assert_eq!(policy.ttl_failed_secs, 2_592_000); // 30 days
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicy {
    /// Prune completed executions older than this, in seconds.
    pub ttl_completed_secs: u64,
    /// Prune failed or aborted executions older than this, in seconds.
    pub ttl_failed_secs: u64,
    /// LRU cap on the number of stored executions.
    pub max_executions: u64,
    /// Size cap on the journal in bytes; exceeding it triggers an LRU sweep.
    pub max_journal_bytes: u64,
    /// Rows deleted per transaction during a prune sweep; the task yields between batches.
    pub prune_batch_size: u64,
    /// Background prune poll interval, in seconds.
    pub prune_interval_secs: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            ttl_completed_secs: 604_800,
            ttl_failed_secs: 2_592_000,
            max_executions: 10_000,
            max_journal_bytes: 1_073_741_824,
            prune_batch_size: 500,
            prune_interval_secs: 3600,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_yields_every_spec_default() {
        let cfg: DurableConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.backend, DurableBackend::Local);
        assert!(cfg.encrypt_payload);
        assert!(!cfg.shared_db);
        assert!(cfg.agent_turns);
        assert!(cfg.orchestration);
        assert!(cfg.scheduler);
        assert!(cfg.subagent);
        assert_eq!(cfg.journal_flush_interval_ms, 10);
        assert_eq!(cfg.journal_ack_timeout_ms, 5000);
        assert_eq!(cfg.max_steps_per_execution, 10_000);
        assert_eq!(cfg.max_payload_bytes, 1_048_576);
        assert_eq!(cfg.promise_poll_interval_secs, 2);
        assert_eq!(cfg.max_parked_promises, 1000);
    }

    #[test]
    fn empty_table_yields_retention_defaults() {
        let cfg: DurableConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.retention.ttl_completed_secs, 604_800);
        assert_eq!(cfg.retention.ttl_failed_secs, 2_592_000);
        assert_eq!(cfg.retention.max_executions, 10_000);
        assert_eq!(cfg.retention.max_journal_bytes, 1_073_741_824);
        assert_eq!(cfg.retention.prune_batch_size, 500);
        assert_eq!(cfg.retention.prune_interval_secs, 3600);
    }

    #[test]
    fn default_impl_matches_serde_default() {
        let from_toml: DurableConfig = toml::from_str("").unwrap();
        assert_eq!(from_toml, DurableConfig::default());
    }

    #[test]
    fn partial_table_overrides_only_named_fields() {
        let cfg: DurableConfig = toml::from_str(
            r#"
            enabled = true
            backend = "restate"

            [retention]
            prune_batch_size = 999
            "#,
        )
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.backend, DurableBackend::Restate);
        // Untouched fields keep their defaults.
        assert_eq!(cfg.journal_ack_timeout_ms, 5000);
        assert_eq!(cfg.retention.prune_batch_size, 999);
        assert_eq!(cfg.retention.ttl_completed_secs, 604_800);
    }

    /// Round-trips the INV-8 forbidden combination this issue's fix must reject at the
    /// `encryption_gate` call site: `encrypt_payload = false` declared alongside `shared_db =
    /// true`. This module only owns the pure data — the gate itself lives in `zeph-durable` — but
    /// the config layer must still deserialize both fields correctly for the gate to see them.
    #[test]
    fn shared_db_and_disabled_encryption_round_trip_together() {
        let cfg: DurableConfig = toml::from_str(
            r"
            encrypt_payload = false
            shared_db = true
            ",
        )
        .unwrap();
        assert!(!cfg.encrypt_payload);
        assert!(cfg.shared_db);
    }
}
