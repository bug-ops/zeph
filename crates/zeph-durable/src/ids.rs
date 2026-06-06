// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Journal-boundary newtypes.
//!
//! Every identifier that crosses the journal boundary is a distinct newtype with private fields
//! and a smart constructor. No raw `String` or `i64` is passed across the API, which makes it
//! impossible to confuse, say, a [`JournalSeq`] with a [`StepId`]. Each newtype is serde-round-trip
//! stable so it can be persisted and reloaded without loss.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Domain-separation context for [`IdempotencyKey`] derivation.
///
/// Passed to BLAKE3's `derive_key` mode so an idempotency key can never collide with a hash
/// produced for any other purpose, even under identical key material.
const IDEMPOTENCY_CONTEXT: &str = "zeph-durable v1 idempotency-key 2026";

/// Identifier of a single durable execution.
///
/// Runtime-minted as a `UUIDv7` (time-ordered) at execution start. It is **never** consumer-supplied
/// for a fresh execution — a resumed execution reuses the persisted value, but a new one always
/// calls [`ExecutionId::new`].
///
/// # Examples
///
/// ```
/// use zeph_durable::ExecutionId;
///
/// let a = ExecutionId::new();
/// let b = ExecutionId::new();
/// assert_ne!(a, b, "each execution gets a distinct identity");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionId(Uuid);

impl ExecutionId {
    /// Mint a fresh, time-ordered execution identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Return the underlying UUID.
    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }

    /// Return the 16 raw bytes of the underlying UUID.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl Default for ExecutionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Position of a step within an execution.
///
/// Assigned at the moment a step is *called* (the Nth call in program order is `StepId(N)`), never
/// at completion, so the value is stable across replays regardless of concurrent completion order
/// (INV-2). Wraps a [`u32`]: an execution is capped well below `u32::MAX` steps by the retention
/// policy.
///
/// # Examples
///
/// ```
/// use zeph_durable::StepId;
///
/// let step = StepId::new(7);
/// assert_eq!(step.value(), 7);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StepId(u32);

impl StepId {
    /// Wrap a raw step position.
    ///
    /// The value normally comes from the execution's atomic step counter; this constructor exists
    /// for the journal backend and tests that reconstruct a persisted step.
    #[must_use]
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    /// Return the raw step position.
    #[must_use]
    pub fn value(self) -> u32 {
        self.0
    }
}

impl fmt::Display for StepId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Global append order of a journal entry — the durability anchor.
///
/// Assigned by the database (an autoincrement / `BIGSERIAL` column), so it is monotonically
/// increasing across all entries of all executions in a journal. Wraps an [`i64`] to match the
/// column type.
///
/// # Examples
///
/// ```
/// use zeph_durable::JournalSeq;
///
/// let first = JournalSeq::new(1);
/// let second = JournalSeq::new(2);
/// assert!(second > first);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JournalSeq(i64);

impl JournalSeq {
    /// Wrap a database-assigned sequence number.
    #[must_use]
    pub fn new(value: i64) -> Self {
        Self(value)
    }

    /// Return the raw sequence number.
    #[must_use]
    pub fn value(self) -> i64 {
        self.0
    }
}

/// Domain-separated deduplication key for a non-idempotent effect.
///
/// Derived with BLAKE3 in `derive_key` mode from `(execution_id, step_id, op_fingerprint)`. The
/// derivation is injective (length-delimited input) so an attacker-controlled `op_fingerprint`
/// cannot be crafted to collide with a different `(execution_id, step_id)` pair. The key is a
/// *deduplication discriminator only* — never the sole trust basis for skipping a guarded effect.
///
/// # Examples
///
/// ```
/// use zeph_durable::{ExecutionId, IdempotencyKey, StepId};
///
/// let exec = ExecutionId::new();
/// let a = IdempotencyKey::derive(exec, StepId::new(0), b"transfer:acct-7");
/// let b = IdempotencyKey::derive(exec, StepId::new(0), b"transfer:acct-7");
/// let c = IdempotencyKey::derive(exec, StepId::new(1), b"transfer:acct-7");
/// assert_eq!(a, b, "same inputs derive the same key");
/// assert_ne!(a, c, "a different step derives a different key");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyKey([u8; 32]);

impl IdempotencyKey {
    /// Derive an idempotency key from the execution identity, step position, and an opaque
    /// operation fingerprint.
    ///
    /// The fingerprint MUST be derived from non-secret descriptors only (e.g. a tool name and its
    /// non-secret arguments); resolved secret material MUST NOT be passed here (INV-6).
    ///
    /// The input is length-delimited — `len(execution_id) || execution_id || len(step_id) ||
    /// step_id || op_fingerprint` — so the field boundaries are unambiguous and the derivation is
    /// injective. The fixed BLAKE3 `derive_key` context string keeps these keys disjoint from any
    /// other BLAKE3 use in the workspace.
    #[must_use]
    pub fn derive(execution_id: ExecutionId, step_id: StepId, op_fingerprint: &[u8]) -> Self {
        let exec_bytes = execution_id.as_bytes();
        let step_bytes = step_id.value().to_le_bytes();
        debug_assert_eq!(exec_bytes.len(), 16, "UUID is always 16 bytes");
        debug_assert_eq!(step_bytes.len(), 4, "u32 is always 4 bytes");

        // Length-prefix each fixed-width field (injective framing); the variable-length
        // op_fingerprint is appended last, where its boundary is unambiguous.
        let mut input = Vec::with_capacity(4 + 16 + 4 + 4 + op_fingerprint.len());
        input.extend_from_slice(&16u32.to_le_bytes());
        input.extend_from_slice(exec_bytes);
        input.extend_from_slice(&4u32.to_le_bytes());
        input.extend_from_slice(&step_bytes);
        input.extend_from_slice(op_fingerprint);

        Self(blake3::derive_key(IDEMPOTENCY_CONTEXT, &input))
    }

    /// Return the 32 raw key bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Reconstruct a key from its 32 stored bytes.
    ///
    /// Used by a journal backend to rebuild a key read back from storage; the bytes MUST originate
    /// from a prior [`IdempotencyKey::as_bytes`] of a key produced by [`IdempotencyKey::derive`].
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// Reference to an external-completion handle (HITL, A2A async, subagent result).
///
/// A `PromiseId` is **not** a bearer capability: resolving a promise additionally requires a
/// separate high-entropy resolver token (INV-9). The id is a `UUIDv7` so it is unguessable for
/// practical purposes and time-ordered for indexing.
///
/// # Examples
///
/// ```
/// use zeph_durable::PromiseId;
///
/// let id = PromiseId::new();
/// assert_ne!(id, PromiseId::new());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PromiseId(Uuid);

impl PromiseId {
    /// Mint a fresh promise identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Return the underlying UUID.
    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for PromiseId {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle to a durable timer that wakes at a persisted instant, surviving process restarts.
///
/// # Examples
///
/// ```
/// use zeph_durable::TimerId;
///
/// let id = TimerId::new();
/// assert_ne!(id, TimerId::new());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TimerId(Uuid);

impl TimerId {
    /// Mint a fresh timer identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Return the underlying UUID.
    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for TimerId {
    fn default() -> Self {
        Self::new()
    }
}

/// Closed classification of what a durable execution represents.
///
/// A closed enum (rather than a free-form string) prevents typos and lets the retention policy
/// reason about execution categories. The `Custom` variant carries a compile-time string literal
/// for execution kinds defined outside the standard set.
///
/// # Examples
///
/// ```
/// use zeph_durable::ExecutionKind;
///
/// assert_eq!(ExecutionKind::AgentTurn.as_str(), "agent_turn");
/// assert_eq!(ExecutionKind::Custom("nightly_sweep").as_str(), "nightly_sweep");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionKind {
    /// A single agent reasoning turn (the P1 adapter target).
    AgentTurn,
    /// An orchestration DAG run (the P2 adapter target).
    DagRun,
    /// A scheduler job fire (the P3 adapter target).
    ScheduledJob,
    /// A subagent session (the P4 adapter target).
    SubagentSession,
    /// A caller-defined execution kind identified by a compile-time literal.
    Custom(&'static str),
}

impl ExecutionKind {
    /// Return the canonical lower-snake-case string used in the `kind` journal column.
    ///
    /// For [`ExecutionKind::Custom`] the inner literal is returned verbatim.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AgentTurn => "agent_turn",
            Self::DagRun => "dag_run",
            Self::ScheduledJob => "scheduled_job",
            Self::SubagentSession => "subagent_session",
            Self::Custom(name) => name,
        }
    }

    /// Reconstruct a standard execution kind from its canonical column string.
    ///
    /// Returns `None` for an unrecognized tag. [`ExecutionKind::Custom`] cannot round-trip from
    /// storage — its inner `&'static str` has no representation recoverable from a dynamic database
    /// string — so a custom kind read back from the journal is reported as unrecognized rather than
    /// silently coerced.
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "agent_turn" => Some(Self::AgentTurn),
            "dag_run" => Some(Self::DagRun),
            "scheduled_job" => Some(Self::ScheduledJob),
            "subagent_session" => Some(Self::SubagentSession),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_id_new_is_unique() {
        let a = ExecutionId::new();
        let b = ExecutionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn promise_and_timer_ids_are_unique() {
        assert_ne!(PromiseId::new(), PromiseId::new());
        assert_ne!(TimerId::new(), TimerId::new());
    }

    #[test]
    fn execution_id_serde_round_trip() {
        let id = ExecutionId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: ExecutionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn step_id_serde_round_trip_and_accessor() {
        let step = StepId::new(42);
        assert_eq!(step.value(), 42);
        let json = serde_json::to_string(&step).unwrap();
        let back: StepId = serde_json::from_str(&json).unwrap();
        assert_eq!(step, back);
    }

    #[test]
    fn journal_seq_serde_round_trip_and_ordering() {
        let seq = JournalSeq::new(99);
        assert_eq!(seq.value(), 99);
        assert!(JournalSeq::new(2) > JournalSeq::new(1));
        let json = serde_json::to_string(&seq).unwrap();
        let back: JournalSeq = serde_json::from_str(&json).unwrap();
        assert_eq!(seq, back);
    }

    #[test]
    fn promise_and_timer_serde_round_trip() {
        let promise = PromiseId::new();
        let timer = TimerId::new();
        let pj = serde_json::to_string(&promise).unwrap();
        let tj = serde_json::to_string(&timer).unwrap();
        assert_eq!(promise, serde_json::from_str::<PromiseId>(&pj).unwrap());
        assert_eq!(timer, serde_json::from_str::<TimerId>(&tj).unwrap());
    }

    #[test]
    fn idempotency_key_serde_round_trip() {
        let key = IdempotencyKey::derive(ExecutionId::new(), StepId::new(3), b"op");
        let json = serde_json::to_string(&key).unwrap();
        let back: IdempotencyKey = serde_json::from_str(&json).unwrap();
        assert_eq!(key, back);
    }

    #[test]
    fn idempotency_key_is_deterministic() {
        let exec = ExecutionId::new();
        let a = IdempotencyKey::derive(exec, StepId::new(5), b"tool:read");
        let b = IdempotencyKey::derive(exec, StepId::new(5), b"tool:read");
        assert_eq!(a, b);
    }

    #[test]
    fn idempotency_key_varies_with_each_input() {
        let exec = ExecutionId::new();
        let other = ExecutionId::new();
        let base = IdempotencyKey::derive(exec, StepId::new(0), b"op");
        assert_ne!(base, IdempotencyKey::derive(other, StepId::new(0), b"op"));
        assert_ne!(base, IdempotencyKey::derive(exec, StepId::new(1), b"op"));
        assert_ne!(base, IdempotencyKey::derive(exec, StepId::new(0), b"op2"));
    }

    #[test]
    fn idempotency_key_framing_is_injective() {
        // The length-delimited framing keeps the step_id/op_fingerprint boundary unambiguous:
        // moving the step bytes into the fingerprint must change the derived key. A naive
        // concatenation that merged the two fields could collide here.
        let exec = ExecutionId::new();
        let with_step = IdempotencyKey::derive(exec, StepId::new(2), b"");
        let with_fingerprint = IdempotencyKey::derive(exec, StepId::new(0), &2u32.to_le_bytes());
        assert_ne!(with_step, with_fingerprint);
    }

    #[test]
    fn execution_kind_as_str_is_stable() {
        assert_eq!(ExecutionKind::AgentTurn.as_str(), "agent_turn");
        assert_eq!(ExecutionKind::DagRun.as_str(), "dag_run");
        assert_eq!(ExecutionKind::ScheduledJob.as_str(), "scheduled_job");
        assert_eq!(ExecutionKind::SubagentSession.as_str(), "subagent_session");
        assert_eq!(ExecutionKind::Custom("x").as_str(), "x");
    }
}
