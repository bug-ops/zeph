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
}
