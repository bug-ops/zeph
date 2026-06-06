// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The crate-wide error type.
//!
//! [`DurableError`] never carries payload bytes or resolver tokens in its messages (INV-5): every
//! variant reports metadata only, so an error can be logged without leaking sealed content.

use crate::ids::StepId;

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
}
