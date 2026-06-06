// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The confidentiality and integrity boundary for journaled payloads.
//!
//! Journal payloads (step results, promise resolutions, checkpoint snapshots) are written to a
//! database file that — for shared-DB and Restate deployments — sits outside the process trust
//! boundary. This module defines the *contract* that protects them:
//!
//! - [`PayloadCipher`] — the AEAD seal/open trait. The concrete `XChaCha20-Poly1305` implementation
//!   lives in a consuming crate (the binary or a `zeph-core`-side module), keyed from the vault, so
//!   `zeph-durable` stays a pure Layer-0 abstraction with no cryptographic dependency (INV-1). The
//!   backend receives the cipher as `Option<Arc<dyn PayloadCipher>>` at construction.
//! - [`PayloadAad`] — the associated data bound into every seal. Binding
//!   `(execution_id, step_id, entry_kind, idem_key)` makes a sealed blob un-relocatable: a result
//!   sealed for one step cannot be opened as the result of another step or another execution
//!   (fail-closed → [`CipherError::Authentication`] → [`DurableError::ReplayIntegrity`]).
//! - [`EntryKindTag`] — a `Copy` discriminator for the entry shape, used inside the AAD so the
//!   cipher never needs to see the payload-bearing [`crate::EntryKind`] itself.
//! - [`CipherError`] — seal/open failures, reported as metadata only (INV-5): no payload bytes,
//!   nonces, or key material ever appear in an error.
//! - [`ensure_payload_within_limit`] — the read-side size guard (INV-11) that fails closed *before*
//!   any decryption or decode is attempted.
//!
//! # Stored blob layout
//!
//! A concrete cipher MUST produce `key_id(1) || nonce(24) || ciphertext || tag(16)`. The leading
//! key-id byte selects the key during a rotation window; the 24-byte nonce is the `XChaCha20`
//! extended nonce, freshly drawn from a CSPRNG on every seal (INV-7).
//!
//! # Examples
//!
//! ```
//! use zeph_durable::{ExecutionId, StepId};
//! use zeph_durable::cipher::{EntryKindTag, PayloadAad};
//!
//! // The AAD for a step result binds the execution, the step, and the entry shape.
//! let aad = PayloadAad::new(ExecutionId::new(), StepId::new(7), EntryKindTag::StepResult, None);
//!
//! // The canonical encoding is deterministic and injective — the same logical AAD always
//! // produces the same bytes, and no two distinct AADs collide.
//! assert_eq!(aad.canonical_bytes(), aad.canonical_bytes());
//! ```

use crate::error::DurableError;
use crate::ids::{ExecutionId, IdempotencyKey, StepId};

/// Wire-format version for [`PayloadAad::canonical_bytes`].
///
/// Bumping this changes the associated-data encoding and is therefore a breaking change for any
/// already-sealed journal (decryption of old entries would fail authentication). It is the first
/// byte of the canonical encoding so the format is self-describing.
const AAD_FORMAT_V1: u8 = 1;

/// Encrypts and decrypts opaque journal payloads with an AEAD construction.
///
/// A `PayloadCipher` is the only component permitted to see plaintext payload bytes. It is injected
/// into a backend as `Option<Arc<dyn PayloadCipher>>`: `None` disables encryption (a development
/// override permitted only for a single-user local backend, see
/// [`DurableConfig::encryption_gate`](crate::DurableConfig::encryption_gate)).
///
/// # Contract for implementors
///
/// - [`seal`](PayloadCipher::seal) MUST draw a fresh CSPRNG nonce for every call (INV-7) and emit
///   the `key_id(1) || nonce(24) || ciphertext || tag(16)` layout.
/// - The `aad` MUST be authenticated via the AEAD's associated-data channel (not merely prepended),
///   so a tampered or relocated entry fails [`open`](PayloadCipher::open).
/// - Neither method may panic on malformed input; corruption is reported as a [`CipherError`].
/// - Implementations are `Send + Sync` so a single cipher can be shared across the writer and
///   replay tasks behind an `Arc`.
///
/// # Examples
///
/// A minimal (insecure, illustrative) implementation that shows the layout discipline a real
/// cipher must follow:
///
/// ```
/// use std::sync::Arc;
/// use zeph_durable::cipher::{CipherError, PayloadAad, PayloadCipher};
///
/// struct Identity;
/// impl PayloadCipher for Identity {
///     fn seal(&self, plaintext: &[u8], _aad: &PayloadAad) -> Result<Vec<u8>, CipherError> {
///         Ok(plaintext.to_vec()) // a real cipher would AEAD-encrypt here
///     }
///     fn open(&self, sealed: &[u8], _aad: &PayloadAad) -> Result<Vec<u8>, CipherError> {
///         Ok(sealed.to_vec())
///     }
/// }
///
/// let cipher: Arc<dyn PayloadCipher> = Arc::new(Identity);
/// assert!(cipher.seal(b"hello", &PayloadAad::detached()).is_ok());
/// ```
pub trait PayloadCipher: Send + Sync {
    /// Seal `plaintext` under `aad`, returning the stored blob
    /// (`key_id || nonce || ciphertext || tag`).
    ///
    /// # Errors
    ///
    /// Returns [`CipherError::Authentication`] if the underlying AEAD encryption fails (an
    /// unexpected condition for a correctly-sized key and nonce).
    fn seal(&self, plaintext: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError>;

    /// Open a blob previously produced by [`seal`](PayloadCipher::seal), verifying `aad`.
    ///
    /// # Errors
    ///
    /// - [`CipherError::Authentication`] if the tag does not verify under `aad` — the entry was
    ///   forged, moved to a different step, or replayed under a different execution.
    /// - [`CipherError::Malformed`] if the blob is too short to contain the framing.
    /// - [`CipherError::UnknownKeyId`] if the leading key-id selects no registered key.
    fn open(&self, sealed: &[u8], aad: &PayloadAad) -> Result<Vec<u8>, CipherError>;
}

/// A `Copy` discriminator naming the shape of a journal entry, used inside [`PayloadAad`].
///
/// It mirrors the variants of [`crate::EntryKind`] without their data, so the cipher can bind the
/// entry shape into the AAD without depending on the payload-bearing enum. The canonical
/// [`as_str`](EntryKindTag::as_str) value matches [`crate::EntryKind::tag`].
///
/// # Examples
///
/// ```
/// use zeph_durable::cipher::EntryKindTag;
///
/// assert_eq!(EntryKindTag::StepResult.as_str(), "step_result");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKindTag {
    /// A committed step result.
    StepResult,
    /// An exactly-once effect intent.
    EffectIntent,
    /// Creation of an external-completion promise.
    PromiseCreated,
    /// Resolution of a promise.
    PromiseResolved,
    /// A durable timer was armed.
    TimerArmed,
    /// A durable timer fired.
    TimerFired,
    /// A compaction checkpoint.
    Checkpoint,
}

impl EntryKindTag {
    /// Return the canonical lower-snake-case tag, identical to [`crate::EntryKind::tag`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StepResult => "step_result",
            Self::EffectIntent => "effect_intent",
            Self::PromiseCreated => "promise_created",
            Self::PromiseResolved => "promise_resolved",
            Self::TimerArmed => "timer_armed",
            Self::TimerFired => "timer_fired",
            Self::Checkpoint => "checkpoint",
        }
    }

    /// A stable single-byte code used in the AAD framing.
    ///
    /// Distinct from the variant's source order so reordering the enum cannot silently change the
    /// wire format.
    const fn aad_code(self) -> u8 {
        match self {
            Self::StepResult => 1,
            Self::EffectIntent => 2,
            Self::PromiseCreated => 3,
            Self::PromiseResolved => 4,
            Self::TimerArmed => 5,
            Self::TimerFired => 6,
            Self::Checkpoint => 7,
        }
    }
}

/// The associated data bound into a payload seal.
///
/// Binding the payload to its location — `(execution_id, step_id, entry_kind, idem_key)` — is what
/// makes a sealed blob un-relocatable. Moving a `StepResult` blob to a different `step_id`, or
/// replaying it under a different `execution_id`, changes the AAD and makes
/// [`PayloadCipher::open`] fail authentication (fail-closed). The fields are private; construct via
/// [`PayloadAad::new`] and read the bound encoding via [`PayloadAad::canonical_bytes`].
///
/// # Security
///
/// The bound `idem_key` and the plaintext payload MUST be derived from non-secret descriptors only
/// (INV-6): resolved secret material is referenced by vault key name, never embedded here or in the
/// [`IdempotencyKey`] fingerprint. The AAD is authenticated but not encrypted, so it must never
/// carry a secret value.
///
/// # Examples
///
/// ```
/// use zeph_durable::{ExecutionId, IdempotencyKey, StepId};
/// use zeph_durable::cipher::{EntryKindTag, PayloadAad};
///
/// let exec = ExecutionId::new();
/// let key = IdempotencyKey::derive(exec, StepId::new(0), b"tool:transfer");
/// let with_key = PayloadAad::new(exec, StepId::new(0), EntryKindTag::StepResult, Some(key));
/// let without_key = PayloadAad::new(exec, StepId::new(0), EntryKindTag::StepResult, None);
///
/// // The optional idempotency key is part of the binding.
/// assert_ne!(with_key.canonical_bytes(), without_key.canonical_bytes());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadAad {
    execution_id: ExecutionId,
    step_id: StepId,
    entry_kind: EntryKindTag,
    idem_key: Option<IdempotencyKey>,
}

impl PayloadAad {
    /// Construct the associated data for a payload at a known journal location.
    #[must_use]
    pub fn new(
        execution_id: ExecutionId,
        step_id: StepId,
        entry_kind: EntryKindTag,
        idem_key: Option<IdempotencyKey>,
    ) -> Self {
        Self {
            execution_id,
            step_id,
            entry_kind,
            idem_key,
        }
    }

    /// A placeholder AAD for doc examples and unit tests that do not exercise binding.
    ///
    /// Not for production use: every real seal MUST bind a meaningful location.
    #[doc(hidden)]
    #[must_use]
    pub fn detached() -> Self {
        Self::new(
            ExecutionId::new(),
            StepId::new(0),
            EntryKindTag::StepResult,
            None,
        )
    }

    /// Encode the AAD as deterministic, injective bytes for the AEAD associated-data channel.
    ///
    /// Layout (fixed positions, so the encoding is injective without per-field length prefixes):
    /// `version(1) || execution_id(16) || step_id_le(4) || entry_kind(1) || idem_present(1) ||
    /// [idem_key(32) when present]`. Every concrete [`PayloadCipher`] feeds these exact bytes to its
    /// AEAD so seal and open agree on the binding.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_durable::{ExecutionId, StepId};
    /// use zeph_durable::cipher::{EntryKindTag, PayloadAad};
    ///
    /// let aad = PayloadAad::new(ExecutionId::new(), StepId::new(1), EntryKindTag::Checkpoint, None);
    /// // version + 16 + 4 + 1 + 1 = 23 bytes when no idempotency key is bound.
    /// assert_eq!(aad.canonical_bytes().len(), 23);
    /// ```
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(23 + if self.idem_key.is_some() { 32 } else { 0 });
        out.push(AAD_FORMAT_V1);
        out.extend_from_slice(self.execution_id.as_bytes());
        out.extend_from_slice(&self.step_id.value().to_le_bytes());
        out.push(self.entry_kind.aad_code());
        match &self.idem_key {
            Some(key) => {
                out.push(1);
                out.extend_from_slice(key.as_bytes());
            }
            None => out.push(0),
        }
        out
    }
}

/// A failure raised by a [`PayloadCipher`].
///
/// Like [`DurableError`], a `CipherError` carries metadata only — never payload bytes, nonces, or
/// key material (INV-5) — so it is always safe to log. The enum is `#[non_exhaustive]`: a concrete
/// cipher may surface additional failure modes in future revisions.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CipherError {
    /// The AEAD tag did not verify: the entry was forged, relocated, or replayed under a different
    /// execution. Maps to [`DurableError::ReplayIntegrity`].
    #[error("sealed payload failed AEAD authentication")]
    Authentication,

    /// The stored blob is too short or otherwise structurally invalid before decryption.
    #[error("sealed blob is malformed: {context}")]
    Malformed {
        /// A non-sensitive description of the structural problem.
        context: &'static str,
    },

    /// The blob's leading key-id selects no key registered with the cipher (e.g. a stale key was
    /// removed before its rotation window closed).
    #[error("no cipher key registered for key-id {key_id}")]
    UnknownKeyId {
        /// The unrecognized key-id byte.
        key_id: u8,
    },
}

impl From<CipherError> for DurableError {
    /// Lift a cipher failure into the crate-wide error, preserving fail-closed semantics.
    ///
    /// An authentication failure is a replay-integrity violation; a structural or key-selection
    /// failure is a decode failure. Both fail closed — no plaintext is ever returned.
    fn from(err: CipherError) -> Self {
        match err {
            CipherError::Authentication => Self::ReplayIntegrity,
            CipherError::Malformed { context } => Self::Decode { context },
            CipherError::UnknownKeyId { .. } => Self::Decode {
                context: "unknown cipher key-id",
            },
        }
    }
}

/// Reject a payload that exceeds `max_bytes` *before* any decryption or decode is attempted.
///
/// This is the read-side half of the `max_payload_bytes` limit (INV-11): a corrupt or hostile
/// journal entry advertising a multi-gigabyte payload is refused in O(1) — no allocation, no
/// decode, no panic — so it cannot be used to exhaust memory. The write side enforces the same
/// limit when an entry is appended.
///
/// # Errors
///
/// Returns [`DurableError::PayloadTooLarge`] when `len` exceeds `max_bytes`.
///
/// # Examples
///
/// ```
/// use zeph_durable::cipher::ensure_payload_within_limit;
///
/// assert!(ensure_payload_within_limit(1024, 1_048_576).is_ok());
/// assert!(ensure_payload_within_limit(2_000_000, 1_048_576).is_err());
/// ```
pub fn ensure_payload_within_limit(len: usize, max_bytes: u64) -> Result<(), DurableError> {
    let size = len as u64;
    if size > max_bytes {
        return Err(DurableError::PayloadTooLarge {
            size,
            max: max_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_key(exec: ExecutionId) -> IdempotencyKey {
        IdempotencyKey::derive(exec, StepId::new(0), b"op")
    }

    #[test]
    fn entry_kind_tag_strings_are_stable() {
        assert_eq!(EntryKindTag::StepResult.as_str(), "step_result");
        assert_eq!(EntryKindTag::EffectIntent.as_str(), "effect_intent");
        assert_eq!(EntryKindTag::PromiseCreated.as_str(), "promise_created");
        assert_eq!(EntryKindTag::PromiseResolved.as_str(), "promise_resolved");
        assert_eq!(EntryKindTag::TimerArmed.as_str(), "timer_armed");
        assert_eq!(EntryKindTag::TimerFired.as_str(), "timer_fired");
        assert_eq!(EntryKindTag::Checkpoint.as_str(), "checkpoint");
    }

    #[test]
    fn entry_kind_tag_aad_codes_are_distinct() {
        let tags = [
            EntryKindTag::StepResult,
            EntryKindTag::EffectIntent,
            EntryKindTag::PromiseCreated,
            EntryKindTag::PromiseResolved,
            EntryKindTag::TimerArmed,
            EntryKindTag::TimerFired,
            EntryKindTag::Checkpoint,
        ];
        let mut codes: Vec<u8> = tags.iter().map(|t| t.aad_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), tags.len(), "every tag has a distinct AAD code");
    }

    #[test]
    fn canonical_bytes_is_deterministic() {
        let aad = PayloadAad::new(
            ExecutionId::new(),
            StepId::new(3),
            EntryKindTag::StepResult,
            None,
        );
        assert_eq!(aad.canonical_bytes(), aad.canonical_bytes());
    }

    #[test]
    fn canonical_bytes_length_matches_idem_presence() {
        let exec = ExecutionId::new();
        let without = PayloadAad::new(exec, StepId::new(0), EntryKindTag::StepResult, None);
        let with = PayloadAad::new(
            exec,
            StepId::new(0),
            EntryKindTag::StepResult,
            Some(sample_key(exec)),
        );
        assert_eq!(without.canonical_bytes().len(), 23);
        assert_eq!(with.canonical_bytes().len(), 23 + 32);
    }

    #[test]
    fn canonical_bytes_differs_per_field() {
        let exec = ExecutionId::new();
        let other = ExecutionId::new();
        let base = PayloadAad::new(exec, StepId::new(0), EntryKindTag::StepResult, None);

        let diff_exec = PayloadAad::new(other, StepId::new(0), EntryKindTag::StepResult, None);
        let diff_step = PayloadAad::new(exec, StepId::new(1), EntryKindTag::StepResult, None);
        let diff_kind = PayloadAad::new(exec, StepId::new(0), EntryKindTag::PromiseResolved, None);
        let diff_key = PayloadAad::new(
            exec,
            StepId::new(0),
            EntryKindTag::StepResult,
            Some(sample_key(exec)),
        );

        let base_bytes = base.canonical_bytes();
        assert_ne!(base_bytes, diff_exec.canonical_bytes());
        assert_ne!(base_bytes, diff_step.canonical_bytes());
        assert_ne!(base_bytes, diff_kind.canonical_bytes());
        assert_ne!(base_bytes, diff_key.canonical_bytes());
    }

    #[test]
    fn canonical_bytes_is_versioned() {
        let aad = PayloadAad::new(
            ExecutionId::new(),
            StepId::new(0),
            EntryKindTag::StepResult,
            None,
        );
        assert_eq!(aad.canonical_bytes()[0], AAD_FORMAT_V1);
    }

    #[test]
    fn cipher_error_maps_to_durable_error_fail_closed() {
        assert!(matches!(
            DurableError::from(CipherError::Authentication),
            DurableError::ReplayIntegrity
        ));
        assert!(matches!(
            DurableError::from(CipherError::Malformed { context: "x" }),
            DurableError::Decode { context: "x" }
        ));
        assert!(matches!(
            DurableError::from(CipherError::UnknownKeyId { key_id: 9 }),
            DurableError::Decode { .. }
        ));
    }

    #[test]
    fn cipher_error_messages_are_metadata_only() {
        // No payload bytes leak; only the structural key-id is named.
        assert!(
            CipherError::UnknownKeyId { key_id: 42 }
                .to_string()
                .contains("42")
        );
        assert_eq!(
            CipherError::Authentication.to_string(),
            "sealed payload failed AEAD authentication"
        );
    }

    #[test]
    fn payload_limit_guard_fails_closed_without_panic() {
        let max: u64 = 1_048_576;
        assert!(ensure_payload_within_limit(0, max).is_ok());
        assert!(
            ensure_payload_within_limit(1_048_576, max).is_ok(),
            "exactly at the limit is ok"
        );
        let err = ensure_payload_within_limit(1_048_577, max).unwrap_err();
        assert!(matches!(
            err,
            DurableError::PayloadTooLarge { size, max: m } if size == 1_048_577 && m == max
        ));
    }
}
