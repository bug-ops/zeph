// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The crate-wide error type.
//!
//! [`DurableError`] never carries payload bytes or resolver tokens in its messages (INV-5): every
//! variant reports metadata only, so an error can be logged without leaking sealed content.

use crate::ids::{ExecutionId, StepId};

/// An error raised by the durable execution layer.
///
/// The enum is `#[non_exhaustive]`: follow-up issues add variants as runtime behavior lands, and
/// downstream `match` expressions must keep a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DurableError {
    /// The replayed step's descriptor fingerprint did not match the fingerprint journaled for this
    /// [`StepId`] (INV-3). The execution is discarded and restarted fresh rather than returning a
    /// result for a structurally different step.
    #[error("replay divergence at step {step_id}: journaled descriptor fingerprint mismatch")]
    ReplayDivergence {
        /// The step whose fingerprint diverged.
        step_id: StepId,
    },

    /// A destructive or security-relevant `ExactlyOnceGuarded` step was constructed without an
    /// explicit ambiguity policy. The safety decision must be made at the call site, not deferred
    /// to a runtime default.
    #[error("step '{step}' requires an explicit on_ambiguous policy for its effect class")]
    AmbiguityPolicyRequired {
        /// The name of the offending step descriptor.
        step: &'static str,
    },

    /// The journal writer did not acknowledge an append within the configured timeout, or is
    /// otherwise unreachable. The calling path degrades to non-durable mode rather than hanging
    /// (INV-12).
    #[error("journal writer unavailable: append was not acknowledged in time")]
    JournalUnavailable,

    /// A payload exceeded the configured `max_payload_bytes` limit. Enforced on both append and
    /// read; it fails closed and never panics (INV-11).
    #[error("payload of {size} bytes exceeds the {max}-byte limit")]
    PayloadTooLarge {
        /// The size of the offending payload, in bytes.
        size: u64,
        /// The configured maximum payload size, in bytes.
        max: u64,
    },

    /// A journal entry could not be decoded: corrupt, truncated, or written under an unknown wire
    /// format version. Fails closed.
    #[error("failed to decode journal entry: {context}")]
    Decode {
        /// A non-sensitive description of the decode failure.
        context: &'static str,
    },

    /// AEAD authentication failed when opening a sealed payload: the entry was forged, moved to a
    /// different step, or replayed under a different execution. Fails closed.
    #[error("replay integrity check failed: sealed payload did not authenticate")]
    ReplayIntegrity,

    /// A control entry's row-level HMAC (INV-8) did not verify against a recomputed value: the row
    /// was forged, relocated to a different step/execution, or is missing its HMAC even though the
    /// backend is keyed. Fails closed like [`ReplayIntegrity`](Self::ReplayIntegrity), but for
    /// HMAC-authenticated control entries (`EffectIntent`) rather than AEAD-sealed payloads.
    #[error("control-entry integrity check failed: row HMAC did not authenticate")]
    ControlIntegrity,

    /// An execution exceeded the hard per-execution step cap and was aborted rather than allowed to
    /// grow unboundedly.
    #[error("execution exceeded the step cap of {cap} steps")]
    StepCapExceeded {
        /// The configured hard step cap.
        cap: u32,
    },

    /// AEAD payload encryption was disabled (`encrypt_payload = false`) for a deployment where it
    /// is mandatory — a non-local backend or a shared database (INV-8). The DB-file trust boundary
    /// does not hold in multi-client environments, so this fails closed at startup.
    #[error(
        "AEAD payload encryption is required for the '{context}' deployment and cannot be disabled"
    )]
    EncryptionRequired {
        /// A non-sensitive label for the deployment that mandates encryption (e.g. `"restate"` or
        /// `"shared-database"`).
        context: &'static str,
    },

    /// A journal entry of a kind whose persistence is provided by a higher layer not yet wired into
    /// this backend revision. Promise, timer, and checkpoint entries land with the promise/timer and
    /// retention layers; until then the backend fails closed rather than silently dropping the
    /// entry's kind-specific state.
    #[error("journal persistence for '{kind}' entries is not available in this backend revision")]
    UnsupportedEntryKind {
        /// The `entry_kind` tag of the entry whose persistence is deferred.
        kind: &'static str,
    },

    /// A journal storage operation failed at the database layer (connection, migration, or query).
    ///
    /// The static `op` names the failing operation; the underlying database error is attached as
    /// the error source. Per INV-5 the `Display` message carries only the operation name — the
    /// boxed source never contains plaintext payloads, since every bind is ciphertext, a hash, or a
    /// non-secret descriptor.
    #[error("durable storage operation '{op}' failed")]
    Storage {
        /// The static name of the failing operation (e.g. `"append"`, `"finalize"`, `"open"`).
        op: &'static str,
        /// The underlying database error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A step's operation closure returned an error on a fresh execution. The step did not complete,
    /// so no `StepResult` is journaled; on a later resume the step re-runs (or, for a guarded effect,
    /// its [`OnAmbiguous`](crate::OnAmbiguous) policy applies). The closure's own error is attached
    /// as the source.
    #[error("step '{step}' operation failed")]
    StepFailed {
        /// The name of the step whose operation closure failed.
        step: &'static str,
        /// The closure's underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A guarded step resumed inside the ambiguous window (an `EffectIntent` is journaled but no
    /// `StepResult`) and its policy is [`OnAmbiguous::Fail`](crate::OnAmbiguous::Fail): the layer
    /// refuses to guess whether the irreversible effect fired and surfaces the decision to the
    /// operator instead of re-running or skipping it.
    #[error("step {step_id} resumed in the ambiguous window and its on_ambiguous policy is 'fail'")]
    AmbiguousEffect {
        /// The step caught in the ambiguous window.
        step_id: StepId,
    },

    /// A step result could not be serialized into journal bytes before sealing. The step's value is
    /// the consumer's serializable type, so this indicates a faulty `Serialize` implementation; it
    /// fails closed rather than journaling a partial payload. Per INV-5 only the step name is named.
    #[error("step '{step}' result could not be serialized for the journal")]
    Serialize {
        /// The name of the step whose result failed to serialize.
        step: &'static str,
    },

    /// A promise resolution referenced a promise that has no `durable_promises` row — either never
    /// created, or pruned. Fails closed rather than silently succeeding. Per INV-5 the raw
    /// `PromiseId` is semi-sensitive and is therefore not embedded in the message.
    #[error("promise resolution failed: no such promise")]
    UnknownPromise,

    /// A promise resolution presented a resolver token that did not match the stored hash (INV-9).
    /// The comparison is constant-time, and neither the presented token nor the raw `PromiseId`
    /// appears in the message (INV-5). The pending promise is left untouched.
    #[error("promise resolution rejected: resolver token did not authenticate")]
    PromiseRejected,

    /// [`crate::backend::LocalBackend::open_execution_exclusive`] found another process already
    /// holding the execution's advisory lock (INV-15, #6122).
    ///
    /// Two processes deriving the same `ExecutionId` (e.g. two CLI instances pointed at the same
    /// `memory.sqlite_path` and the same `ConversationId`) can no longer both drive it
    /// concurrently: the second process gets this error instead of silently racing the first into
    /// `ReplayDivergence`/`ReplayIntegrity` failures. Distinct from those two variants so callers
    /// (and operators reading logs) can tell "another live process owns this execution" apart from
    /// "the journal itself is corrupt or was tampered with".
    #[error("execution {execution_id} is already open in another process (pid {holder_pid})")]
    ExecutionLocked {
        /// The execution whose lock is already held.
        execution_id: ExecutionId,
        /// PID of the process currently holding the lock, or `0` if it could not be determined.
        holder_pid: u32,
    },

    /// [`crate::backend::LocalBackend::open_execution`] (or its exclusive variant) found the
    /// execution's row already `canceled` (INV-16′, #6362). Unlike `completed`/`failed`/`aborted`,
    /// a canceled row is never un-finalized and reopened — the cancellation was an explicit
    /// operator decision that this execution must not run again.
    #[error("execution {execution_id} was canceled and cannot be resumed")]
    ExecutionCanceled {
        /// The execution whose row is `canceled`.
        execution_id: ExecutionId,
    },
}

impl DurableError {
    /// Wrap a database-layer failure as a [`DurableError::Storage`] for the named operation.
    ///
    /// Used at every `zeph-db` call site so storage failures carry a stable, greppable operation
    /// label while the original error remains reachable via [`std::error::Error::source`].
    pub(crate) fn storage(
        op: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self::Storage {
            op,
            source: source.into(),
        }
    }

    /// Wrap a step operation closure's failure as a [`DurableError::StepFailed`].
    ///
    /// Keeps the originating error reachable via [`std::error::Error::source`] while the `Display`
    /// line stays metadata-only (INV-5).
    pub(crate) fn step_failed(step: &'static str, source: crate::step::StepError) -> Self {
        Self::StepFailed {
            step,
            source: source.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_metadata_only() {
        let err = DurableError::PayloadTooLarge {
            size: 2_000_000,
            max: 1_048_576,
        };
        let rendered = err.to_string();
        assert!(rendered.contains("2000000"));
        assert!(rendered.contains("1048576"));
    }

    #[test]
    fn replay_divergence_reports_step() {
        let err = DurableError::ReplayDivergence {
            step_id: StepId::new(12),
        };
        assert!(err.to_string().contains("step 12"));
    }

    #[test]
    fn storage_message_names_op_but_not_the_source_detail() {
        let inner = std::io::Error::other("secret-bind-value");
        let err = DurableError::storage("append", inner);
        let rendered = err.to_string();
        assert!(rendered.contains("append"));
        // The top-line message is metadata-only: the source detail is reachable via `source()`,
        // never inlined into Display (INV-5).
        assert!(!rendered.contains("secret-bind-value"));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn step_failed_names_step_but_not_the_source_detail() {
        let err = DurableError::step_failed(
            "transfer_funds",
            crate::step::StepError::new("secret-operation-detail"),
        );
        let rendered = err.to_string();
        assert!(rendered.contains("transfer_funds"));
        assert!(!rendered.contains("secret-operation-detail"));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn ambiguous_and_serialize_messages_are_metadata_only() {
        let ambiguous = DurableError::AmbiguousEffect {
            step_id: StepId::new(4),
        };
        assert!(ambiguous.to_string().contains("step 4"));

        let serialize = DurableError::Serialize { step: "persist" };
        assert!(serialize.to_string().contains("persist"));
    }

    #[test]
    fn execution_locked_names_execution_and_holder_pid() {
        let execution_id = ExecutionId::new();
        let err = DurableError::ExecutionLocked {
            execution_id,
            holder_pid: 4242,
        };
        let rendered = err.to_string();
        assert!(rendered.contains(&execution_id.to_string()));
        assert!(rendered.contains("4242"));
    }

    #[test]
    fn execution_canceled_names_execution_and_is_metadata_only() {
        let execution_id = ExecutionId::new();
        let err = DurableError::ExecutionCanceled { execution_id };
        let rendered = err.to_string();
        assert!(rendered.contains(&execution_id.to_string()));
        assert!(rendered.contains("canceled"));
    }
}
