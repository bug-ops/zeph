// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

// sqlx 0.9 increases the generated Future type depth in generic async step
// wrappers (handle.rs) beyond the default limit of 128.
#![recursion_limit = "256"]

//! Native durable execution layer for Zeph.
//!
//! `zeph-durable` is a Layer-0 infrastructure crate — analogous to `zeph-db` and `zeph-common` —
//! that journals the *control flow* of an execution (individual steps, their inputs and outputs,
//! promises, and timers) so a crashed or interrupted execution can resume at the point of failure
//! rather than restart from scratch.
//!
//! # Architectural placement
//!
//! Consumers of this crate span several layers (`zeph-scheduler`, `zeph-subagent`,
//! `zeph-orchestration`, `zeph-agent-tools`), so the crate must sit at Layer 0. It is a pure
//! infrastructure primitive: it sees opaque serialized payloads, never domain types, and it
//! MUST NOT depend on `zeph-llm`, `zeph-memory`, `zeph-core`, `zeph-sanitizer`, or any
//! business-layer crate (INV-1). Domain meaning lives in thin adapter modules inside each
//! consuming crate.
//!
//! # Module map
//!
//! Type-level foundation:
//!
//! - [`ids`] — the journal-boundary newtypes ([`ExecutionId`], [`StepId`], [`JournalSeq`],
//!   [`IdempotencyKey`], [`PromiseId`], [`TimerId`]) and the [`ExecutionKind`] discriminator.
//! - [`journal`] — the [`Journal`] trait plus the [`JournalEntry`] / [`EntryKind`] /
//!   [`ExecutionStatus`] data model.
//! - [`cipher`] — the [`PayloadCipher`] AEAD contract, [`PayloadAad`] binding, and the read-side
//!   `max_payload` guard. The concrete cipher lives in a consuming crate (INV-1).
//! - [`effect`] — the [`EffectClass`] side-effect contract referenced by journal entries.
//! - [`config`] — re-exports the pure-data [`DurableConfig`] and [`RetentionPolicy`] (which live in
//!   `zeph-config`) and owns the [`encryption_gate`] AEAD enforcement policy.
//! - [`error`] — the crate-wide [`DurableError`].
//!
//! Persistence engine:
//!
//! - [`backend`] — the sealed [`ExecutionBackend`] trait, [`BackendCapabilities`], the
//!   [`DurableBackendEnum`] enum dispatcher, and [`LocalBackend`] (a dedicated `durable.db` pool).
//! - [`writer`] — the background [`JournalWriter`] actor and its cloneable
//!   [`JournalWriterHandle`]: group-commit for buffered appends, flush-before-commit ACKs for
//!   exactly-once entries, and `MAX(seq)` restart resume.
//!
//! Execution surface:
//!
//! - [`step`] — the durable step typestate: [`StepDescriptor`] (with the construction-time
//!   ambiguity rule), [`StepHandle`], [`StepError`], [`StepOutcome`], and [`DurableStep`].
//! - [`handle`] — the `&self` [`DurableContext`] front door: deterministic step ids, replay with a
//!   BLAKE3 divergence guard, the exactly-once intent/result protocol, and [`ParallelScope`] for
//!   completion-order-independent parallel batches.
//!
//! The promise, timer, and retention layers build on these in follow-up issues.
//!
//! # Schema ownership
//!
//! `zeph-durable` owns **no** `.sql` files and **no** `sqlx::migrate!`. All durable schema (the
//! four `durable_*` tables) lives as numbered migration files in
//! `zeph-db/migrations/{sqlite,postgres}/` and is applied via `zeph_db::run_migrations` against a
//! dedicated `durable.db` pool (INV-14).
//!
//! # Examples
//!
//! ```
//! use zeph_durable::{ExecutionId, IdempotencyKey, StepId};
//!
//! // Each execution gets a fresh, runtime-minted identity.
//! let execution = ExecutionId::new();
//!
//! // Idempotency keys are domain-separated and deterministic for a given step.
//! let key = IdempotencyKey::derive(execution, StepId::new(0), b"tool:read_file");
//! assert_eq!(key, IdempotencyKey::derive(execution, StepId::new(0), b"tool:read_file"));
//! ```

mod replay;
mod sealed;
mod waiters;

pub mod backend;
pub mod cipher;
pub mod config;
pub mod effect;
pub mod error;
pub mod handle;
pub mod ids;
pub mod journal;
pub mod promise;
pub mod retention;
pub mod step;
pub mod timer;
pub mod writer;

#[doc(hidden)]
pub use sealed::Sealed;

pub use backend::{
    BackendCapabilities, DurableBackendEnum, ExecutionBackend, ExecutionSummary, LocalBackend,
    RedactedEntry,
};
pub use cipher::{
    CipherError, EntryKindTag, PayloadAad, PayloadCipher, ensure_payload_within_limit,
};
pub use config::{DurableBackend, DurableConfig, EncryptionGate, RetentionPolicy, encryption_gate};
pub use effect::{EffectClass, EffectIntentSubClass, OnAmbiguous};
pub use error::DurableError;
pub use handle::{DurableContext, ParallelScope};
pub use ids::{ExecutionId, ExecutionKind, IdempotencyKey, JournalSeq, PromiseId, StepId, TimerId};
pub use journal::{EntryKind, ExecutionStatus, Journal, JournalEntry};
pub use promise::{DurableHandle, DurablePromise};
pub use retention::DurableRetentionService;
pub use step::{DurableStep, StepDescriptor, StepError, StepHandle, StepOutcome};
pub use timer::DurableTimerService;
pub use writer::{JournalWriter, JournalWriterHandle};
