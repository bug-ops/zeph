// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The durable step primitive and its typestate.
//!
//! A *step* is the unit of durable progress: [`DurableContext::step`](crate::DurableContext::step)
//! runs an operation closure, journals its result, and on a later resume returns the journaled
//! result instead of re-running the closure. This module defines the types that describe and carry a
//! step:
//!
//! - [`StepDescriptor`] — the *what* of a step: its name, [`EffectClass`], ambiguity policy, and an
//!   opaque operation fingerprint. Its constructors enforce the construction-time ambiguity rule
//!   (FR-DE-09): a destructive, security-relevant, money-moving, or custom guarded step that omits
//!   an [`OnAmbiguous`] policy is rejected with [`DurableError::AmbiguityPolicyRequired`].
//! - [`StepHandle`] — handed to the operation closure so it can forward the step's
//!   [`IdempotencyKey`] to an external service as an `Idempotency-Key` header for boundary dedup.
//! - [`StepError`] — the closure's failure channel: any error type the closure produces is wrapped
//!   here without coupling the Layer-0 crate to a consumer's error enum (INV-1).
//! - [`StepOutcome`] — the `Live` / `Replayed` typestate that lets a consumer suppress
//!   already-emitted side effects (e.g. re-printing assistant output) on replay.
//! - [`DurableStep`] — the recorded result of a step: its id, idempotency key, and outcome.
//!
//! The payload codec is JSON: a step value is serialized to bytes, length-checked, then handed to
//! the journal where the backend AEAD-seals it. The bytes are opaque to the journal — the durable
//! layer never inspects a domain type (INV-1).

use std::fmt;
use std::marker::PhantomData;

use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::effect::{EffectClass, EffectIntentSubClass, OnAmbiguous};
use crate::error::DurableError;
use crate::ids::{IdempotencyKey, StepId};

/// Wire-format version stamped on every sealed step payload.
///
/// Stored in the `payload_version` journal column so a future codec change can be detected and
/// migrated rather than silently misread.
pub(crate) const PAYLOAD_VERSION: u8 = 1;

/// The error channel for a step's operation closure.
///
/// The durable layer is Layer-0 infrastructure and must not depend on any consumer's error enum
/// (INV-1), so a closure reports failure through this opaque wrapper. Construct it from any boxable
/// error (or a message) with [`StepError::new`]; the wrapped error stays reachable through
/// [`DurableError::StepFailed`]'s source.
///
/// # Examples
///
/// ```
/// use zeph_durable::StepError;
///
/// // From a message:
/// let _ = StepError::new("provider returned 503");
/// // From a concrete error:
/// let io = std::io::Error::other("disk full");
/// let _ = StepError::new(io);
/// ```
pub struct StepError(Box<dyn std::error::Error + Send + Sync>);

impl StepError {
    /// Wrap any boxable error (including a `&str` or `String` message) as a step failure.
    #[must_use]
    pub fn new(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(source.into())
    }

    /// Consume the wrapper, returning the boxed source error.
    pub(crate) fn into_inner(self) -> Box<dyn std::error::Error + Send + Sync> {
        self.0
    }
}

impl fmt::Debug for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StepError").field(&self.0).finish()
    }
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// The description of a step: its identity, effect contract, and ambiguity policy.
///
/// A descriptor is *what* a step is, independent of *when* it runs. The durable layer derives the
/// step's [`IdempotencyKey`] and replay-divergence fingerprint from the descriptor, so the same
/// program point must build an equal descriptor on every run — a structurally different descriptor
/// at a given [`StepId`] is a [`DurableError::ReplayDivergence`].
///
/// Build a descriptor through the effect-specific constructors; the
/// [`exactly_once_guarded`](StepDescriptor::exactly_once_guarded) constructor enforces the
/// construction-time ambiguity rule (FR-DE-09).
///
/// # Examples
///
/// ```
/// use zeph_durable::{EffectIntentSubClass, OnAmbiguous, StepDescriptor};
///
/// // A read-only step is idempotent and needs no ambiguity policy.
/// let read = StepDescriptor::idempotent("read_file", b"tool:read_file:/etc/hosts".to_vec());
///
/// // A destructive guarded step MUST declare its ambiguity policy or construction fails.
/// let delete = StepDescriptor::exactly_once_guarded(
///     "delete_file",
///     EffectIntentSubClass::Destructive,
///     Some(OnAmbiguous::Fail),
///     b"tool:delete_file:/tmp/x".to_vec(),
/// );
/// assert!(delete.is_ok());
///
/// let unsafe_delete = StepDescriptor::exactly_once_guarded(
///     "delete_file",
///     EffectIntentSubClass::Destructive,
///     None,
///     b"tool:delete_file:/tmp/x".to_vec(),
/// );
/// assert!(unsafe_delete.is_err(), "a destructive guarded step needs an explicit policy");
/// ```
#[derive(Debug, Clone)]
pub struct StepDescriptor {
    name: &'static str,
    effect: EffectClass,
    on_ambiguous: Option<OnAmbiguous>,
    op_fingerprint: Bytes,
}

impl StepDescriptor {
    /// Describe an [`EffectClass::Idempotent`] step (pure or naturally repeatable).
    ///
    /// A replayed idempotent step returns its journaled result and never re-invokes the closure
    /// (INV-10). No ambiguity policy applies.
    #[must_use]
    pub fn idempotent(name: &'static str, op_fingerprint: impl Into<Bytes>) -> Self {
        Self {
            name,
            effect: EffectClass::Idempotent,
            on_ambiguous: None,
            op_fingerprint: op_fingerprint.into(),
        }
    }

    /// Describe an [`EffectClass::AtLeastOnce`] step (safe to repeat under an ambiguous replay).
    #[must_use]
    pub fn at_least_once(name: &'static str, op_fingerprint: impl Into<Bytes>) -> Self {
        Self {
            name,
            effect: EffectClass::AtLeastOnce,
            on_ambiguous: None,
            op_fingerprint: op_fingerprint.into(),
        }
    }

    /// Describe an [`EffectClass::ExactlyOnceGuarded`] step, enforcing the ambiguity-policy rule.
    ///
    /// The `sub_class` refines what the effect does; the resulting [`OnAmbiguous`] policy decides
    /// what happens if a crash leaves the step in the ambiguous window. Only
    /// [`EffectIntentSubClass::CostBearingOrBoundaryIdempotent`] has a safe default
    /// ([`OnAmbiguous::Skip`]); every other sub-class requires an explicit policy.
    ///
    /// # Errors
    ///
    /// Returns [`DurableError::AmbiguityPolicyRequired`] when `sub_class` requires an explicit
    /// policy ([`EffectIntentSubClass::requires_explicit_policy`]) but `on_ambiguous` is `None`.
    pub fn exactly_once_guarded(
        name: &'static str,
        sub_class: EffectIntentSubClass,
        on_ambiguous: Option<OnAmbiguous>,
        op_fingerprint: impl Into<Bytes>,
    ) -> Result<Self, DurableError> {
        let resolved = match on_ambiguous {
            Some(policy) => policy,
            None if sub_class.requires_explicit_policy() => {
                return Err(DurableError::AmbiguityPolicyRequired { step: name });
            }
            // Only the cost-bearing / boundary-idempotent sub-class reaches here: a paid call the
            // external boundary deduplicates by idempotency key is safe to skip on ambiguity.
            None => OnAmbiguous::Skip,
        };
        Ok(Self {
            name,
            effect: EffectClass::ExactlyOnceGuarded,
            on_ambiguous: Some(resolved),
            op_fingerprint: op_fingerprint.into(),
        })
    }

    /// The step's stable name (used in spans, audit records, and error messages).
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The step's effect class.
    #[must_use]
    pub fn effect(&self) -> EffectClass {
        self.effect
    }

    /// The resolved ambiguity policy, `Some` only for a guarded step.
    #[must_use]
    pub fn on_ambiguous(&self) -> Option<OnAmbiguous> {
        self.on_ambiguous
    }

    /// The opaque, non-secret operation fingerprint (INV-6).
    #[must_use]
    pub fn op_fingerprint(&self) -> &Bytes {
        &self.op_fingerprint
    }

    /// The length-delimited structural fingerprint fed to [`IdempotencyKey::derive`].
    ///
    /// Folding `name` and `effect` in (each length-prefixed, the variable `op_fingerprint` last)
    /// makes the derived idempotency key the step's structural identity: changing any of them at a
    /// given [`StepId`] changes the key, which the replay cursor detects as a divergence (INV-3).
    /// The framing is injective so distinct descriptors never collide.
    pub(crate) fn fingerprint_input(&self) -> Vec<u8> {
        let effect = self.effect.as_str();
        let mut input =
            Vec::with_capacity(4 + self.name.len() + 4 + effect.len() + self.op_fingerprint.len());
        input.extend_from_slice(&u32_len(self.name.len()).to_le_bytes());
        input.extend_from_slice(self.name.as_bytes());
        input.extend_from_slice(&u32_len(effect.len()).to_le_bytes());
        input.extend_from_slice(effect.as_bytes());
        input.extend_from_slice(&self.op_fingerprint);
        input
    }
}

/// A length cast that saturates rather than wrapping — fingerprint inputs are tiny, but the cast
/// must never silently truncate a (pathological) oversized field into a colliding length prefix.
fn u32_len(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// A handle passed to a step's operation closure.
///
/// Its purpose is boundary deduplication: the closure reads [`StepHandle::idempotency_key`] and
/// forwards it to an external service (e.g. as an `Idempotency-Key` header) so a re-issued call
/// after an ambiguous crash is deduplicated at the boundary. The handle is `Copy` and carries no
/// secret material.
///
/// # Examples
///
/// ```
/// use zeph_durable::{ExecutionId, IdempotencyKey, StepHandle, StepId};
///
/// # fn demo(handle: StepHandle) {
/// // The closure can thread the idempotency key into an outbound request.
/// let _header_value = handle.idempotency_key();
/// let _which_step = handle.step_id();
/// # }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct StepHandle {
    step_id: StepId,
    idempotency_key: IdempotencyKey,
}

impl StepHandle {
    /// Construct a handle for the closure (crate-internal; built by the durable context).
    pub(crate) fn new(step_id: StepId, idempotency_key: IdempotencyKey) -> Self {
        Self {
            step_id,
            idempotency_key,
        }
    }

    /// The step's deterministic position within its execution.
    #[must_use]
    pub fn step_id(&self) -> StepId {
        self.step_id
    }

    /// The step's idempotency key, suitable for forwarding to an external boundary for dedup.
    #[must_use]
    pub fn idempotency_key(&self) -> IdempotencyKey {
        self.idempotency_key
    }
}

/// Whether a step's value came from a live run or from the journal.
///
/// Both variants carry the same `T`; the discriminator lets a consumer suppress already-emitted
/// side effects on replay (the spec's `RuntimeLayer` double-print suppression) without the durable
/// layer knowing what those side effects are.
///
/// # Examples
///
/// ```
/// use zeph_durable::StepOutcome;
///
/// let live = StepOutcome::Live(7);
/// assert!(!live.was_replayed());
/// assert_eq!(*live.get(), 7);
/// assert_eq!(live.into_inner(), 7);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome<T> {
    /// The operation closure ran on this execution and produced the value.
    Live(T),
    /// The value was returned from the journal; the closure was not invoked.
    Replayed(T),
}

impl<T> StepOutcome<T> {
    /// Whether the value was replayed from the journal rather than freshly computed.
    #[must_use]
    pub fn was_replayed(&self) -> bool {
        matches!(self, Self::Replayed(_))
    }

    /// Borrow the contained value regardless of provenance.
    #[must_use]
    pub fn get(&self) -> &T {
        match self {
            Self::Live(value) | Self::Replayed(value) => value,
        }
    }

    /// Consume the outcome and return the contained value.
    #[must_use]
    pub fn into_inner(self) -> T {
        match self {
            Self::Live(value) | Self::Replayed(value) => value,
        }
    }
}

/// The recorded result of a [`DurableContext::step`](crate::DurableContext::step) call.
///
/// Bundles the step's deterministic identity (its [`StepId`] and [`IdempotencyKey`]) with the
/// [`StepOutcome`]. Most callers want only the value
/// ([`DurableContext::step`](crate::DurableContext::step) returns it directly); take a
/// `DurableStep` when the id, the key, or the live/replayed distinction matters.
///
/// `T` is recorded only as a type witness — the value lives inside the [`StepOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableStep<T> {
    step_id: StepId,
    idempotency_key: IdempotencyKey,
    outcome: StepOutcome<T>,
    _marker: PhantomData<fn() -> T>,
}

impl<T> DurableStep<T> {
    /// Build a record for a freshly-executed step.
    pub(crate) fn live(step_id: StepId, idempotency_key: IdempotencyKey, value: T) -> Self {
        Self {
            step_id,
            idempotency_key,
            outcome: StepOutcome::Live(value),
            _marker: PhantomData,
        }
    }

    /// Build a record for a step whose value was replayed from the journal.
    pub(crate) fn replayed(step_id: StepId, idempotency_key: IdempotencyKey, value: T) -> Self {
        Self {
            step_id,
            idempotency_key,
            outcome: StepOutcome::Replayed(value),
            _marker: PhantomData,
        }
    }

    /// The step's deterministic position within its execution.
    #[must_use]
    pub fn step_id(&self) -> StepId {
        self.step_id
    }

    /// The step's idempotency key.
    #[must_use]
    pub fn idempotency_key(&self) -> IdempotencyKey {
        self.idempotency_key
    }

    /// Whether the value was replayed from the journal.
    #[must_use]
    pub fn was_replayed(&self) -> bool {
        self.outcome.was_replayed()
    }

    /// Borrow the step's value.
    #[must_use]
    pub fn value(&self) -> &T {
        self.outcome.get()
    }

    /// Borrow the full outcome (value plus live/replayed provenance).
    #[must_use]
    pub fn outcome(&self) -> &StepOutcome<T> {
        &self.outcome
    }

    /// Consume the record and return just the value.
    #[must_use]
    pub fn into_value(self) -> T {
        self.outcome.into_inner()
    }

    /// Consume the record and return the full outcome.
    #[must_use]
    pub fn into_outcome(self) -> StepOutcome<T> {
        self.outcome
    }
}

/// Serialize a step value into journal bytes (the codec is JSON; the journal seals these opaquely).
///
/// # Errors
///
/// Returns [`DurableError::Serialize`] if the value cannot be serialized.
pub(crate) fn serialize_result<T: Serialize>(
    value: &T,
    step: &'static str,
) -> Result<Bytes, DurableError> {
    serde_json::to_vec(value)
        .map(Bytes::from)
        .map_err(|_| DurableError::Serialize { step })
}

/// Deserialize journaled bytes back into a step value.
///
/// # Errors
///
/// Returns [`DurableError::Decode`] if the bytes cannot be decoded into `T`.
pub(crate) fn deserialize_result<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DurableError> {
    serde_json::from_slice(bytes).map_err(|_| DurableError::Decode {
        context: "step result payload could not be deserialized into its type",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ExecutionId;

    #[test]
    fn guarded_destructive_requires_explicit_policy() {
        let err = StepDescriptor::exactly_once_guarded(
            "delete",
            EffectIntentSubClass::Destructive,
            None,
            b"op".to_vec(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DurableError::AmbiguityPolicyRequired { step: "delete" }
        ));
    }

    #[test]
    fn guarded_cost_bearing_defaults_to_skip() {
        let desc = StepDescriptor::exactly_once_guarded(
            "llm_call",
            EffectIntentSubClass::CostBearingOrBoundaryIdempotent,
            None,
            b"op".to_vec(),
        )
        .unwrap();
        assert_eq!(desc.on_ambiguous(), Some(OnAmbiguous::Skip));
        assert_eq!(desc.effect(), EffectClass::ExactlyOnceGuarded);
    }

    #[test]
    fn guarded_explicit_policy_overrides_default() {
        let desc = StepDescriptor::exactly_once_guarded(
            "llm_call",
            EffectIntentSubClass::CostBearingOrBoundaryIdempotent,
            Some(OnAmbiguous::Rerun),
            b"op".to_vec(),
        )
        .unwrap();
        assert_eq!(desc.on_ambiguous(), Some(OnAmbiguous::Rerun));
    }

    #[test]
    fn non_guarded_descriptors_have_no_policy() {
        assert_eq!(
            StepDescriptor::idempotent("read", b"op".to_vec()).on_ambiguous(),
            None
        );
        assert_eq!(
            StepDescriptor::at_least_once("enqueue", b"op".to_vec()).on_ambiguous(),
            None
        );
    }

    #[test]
    fn fingerprint_input_is_injective_across_descriptor_fields() {
        let base = StepDescriptor::idempotent("a", b"x".to_vec()).fingerprint_input();
        // A different name with a fingerprint that would naively concatenate to the same bytes must
        // still differ thanks to the length framing.
        let shifted = StepDescriptor::idempotent("ax", b"".to_vec()).fingerprint_input();
        assert_ne!(base, shifted);
        // A different effect class changes the fingerprint even with identical name + op bytes.
        let other_effect = StepDescriptor::at_least_once("a", b"x".to_vec()).fingerprint_input();
        assert_ne!(base, other_effect);
    }

    #[test]
    fn fingerprint_drives_idempotency_key_divergence() {
        let exec = ExecutionId::new();
        let step = StepId::new(0);
        let a = IdempotencyKey::derive(
            exec,
            step,
            &StepDescriptor::idempotent("a", b"x".to_vec()).fingerprint_input(),
        );
        let b = IdempotencyKey::derive(
            exec,
            step,
            &StepDescriptor::idempotent("b", b"x".to_vec()).fingerprint_input(),
        );
        assert_ne!(
            a, b,
            "a different descriptor derives a different idempotency key"
        );
    }

    #[test]
    fn step_outcome_and_durable_step_accessors() {
        let key = IdempotencyKey::derive(ExecutionId::new(), StepId::new(2), b"op");
        let live = DurableStep::live(StepId::new(2), key, 41_u32);
        assert_eq!(live.step_id(), StepId::new(2));
        assert_eq!(live.idempotency_key(), key);
        assert!(!live.was_replayed());
        assert_eq!(*live.value(), 41);
        assert!(matches!(live.outcome(), StepOutcome::Live(41)));
        assert_eq!(live.into_value(), 41);

        let replayed = DurableStep::replayed(StepId::new(3), key, 7_u32);
        assert!(replayed.was_replayed());
        assert!(matches!(replayed.into_outcome(), StepOutcome::Replayed(7)));
    }

    #[test]
    fn payload_codec_round_trips() {
        let bytes = serialize_result(&vec![1_u32, 2, 3], "step").unwrap();
        let back: Vec<u32> = deserialize_result(&bytes).unwrap();
        assert_eq!(back, vec![1, 2, 3]);
    }

    #[test]
    fn deserialize_fails_closed_on_garbage() {
        let err = deserialize_result::<u32>(b"not json").unwrap_err();
        assert!(matches!(err, DurableError::Decode { .. }));
    }

    #[test]
    fn step_error_wraps_message_and_concrete_error() {
        assert_eq!(StepError::new("boom").to_string(), "boom");
        let io = std::io::Error::other("disk full");
        assert!(StepError::new(io).to_string().contains("disk full"));
    }
}
