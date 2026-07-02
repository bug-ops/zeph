// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Conversation-session persistence, event-log replay, and fork engine for Zeph.
//!
//! `zeph-session` implements spec-068: an append-only JSONL event log (the source of truth for
//! a conversation), a metadata index over the existing `acp_sessions` table, a deterministic
//! [`replay::ReplayEngine`], and the [`condenser::Condenser`] contract for durable context
//! condensation. It is consumed by `zeph-core` (agent-loop `SessionSink` wiring, `zeph serve`
//! per-session actors) and `zeph-acp` (session load/list/fork/resume handlers thinned to
//! delegate here).
//!
//! # Architectural placement
//!
//! `zeph-session` mirrors the append-only journal design of `zeph-durable`
//! (sequential event ordering, a single-writer actor model) but is a **separate** crate: the two
//! operate at different abstraction levels (task/step effect-idempotency vs. conversation
//! semantics) and use different storage formats (`SQLite`-backed opaque payloads vs. JSONL
//! domain-typed events). `zeph-session` MUST NOT depend on `zeph-durable`, and vice versa
//! (spec-068 §3, §15 NEVER; INV-1 in spec-064).
//!
//! # Module map
//!
//! - [`error`] — the crate-wide [`error::SessionError`].
//! - [`event`] — the [`event::SessionEvent`] tagged enum and its [`event::SessionEventEnvelope`]
//!   on-disk wrapper. Reuses `zeph_llm::provider::MessagePart` and
//!   `zeph_common::memory::AnchoredSummary` rather than redefining them.
//! - [`log`] — [`log::SessionEventLog`]: the append-only JSONL writer/reader, including the
//!   INV-SP-2 torn-append truncation logic.
//! - [`store`] — [`store::SessionStore`]: CRUD over the `acp_sessions` metadata index (spec §5).
//! - [`replay`] — [`replay::ReplayEngine`]: deterministic fold of an event log into agent-ready
//!   messages. Never calls the LLM or a tool executor.
//! - [`condenser`] — the [`condenser::Condenser`] trait contract and the INV-SP-4 non-overlap
//!   guard.
//! - [`llm_condenser`] — [`llm_condenser::LlmCondenser`]: the default `Condenser` implementation,
//!   reusing `zeph_context::summarization::summarize_structured`.
//! - [`fork`] — [`fork::ForkEngine`]: eager-copy session forking (spec §7).
//!
//! The `zeph-core` `SessionActor` integration (`zeph serve`, spec §9) lands in a later phase of
//! the implementation plan (`specs/068-session-persistence/plan.md`).

pub mod condenser;
pub mod error;
pub mod event;
pub mod fork;
pub mod llm_condenser;
pub mod log;
pub mod replay;
pub mod store;

pub use condenser::{CondensationResult, Condenser};
pub use error::SessionError;
pub use event::{CompactionTier, SessionEvent, SessionEventEnvelope};
pub use fork::{ForkEngine, ForkResult};
pub use llm_condenser::LlmCondenser;
pub use log::SessionEventLog;
pub use replay::{ReconstructedState, ReplayEngine};
pub use store::{SessionFilter, SessionMetadata, SessionStatus, SessionStore};

/// The on-disk directory for one session's event log and blobs, per spec §4.1:
/// `<data_dir>/sessions/<session_id>/`.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// let dir = zeph_session::session_dir(Path::new(".zeph/sessions"), "abc-123");
/// assert_eq!(dir, Path::new(".zeph/sessions/sessions/abc-123"));
/// ```
#[must_use]
pub fn session_dir(data_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    data_dir.join("sessions").join(session_id)
}
