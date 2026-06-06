// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The per-step side-effect contract.
//!
//! [`EffectClass`] declares how a step's side effect behaves under replay. It is the foundation of
//! the exactly-once machinery: the replay cursor uses it to decide whether a journaled result may
//! be returned without re-running the operation.
//!
//! For [`EffectClass::ExactlyOnceGuarded`] steps the contract is sharper: an
//! [`EffectIntentSubClass`] further classifies *what kind* of side effect the step performs, and an
//! [`OnAmbiguous`] policy decides what to do when a crash leaves the journal in the *ambiguous
//! window* — an `EffectIntent` committed, but no `StepResult`, so it is unknown whether the external
//! effect actually fired. The combination is enforced at construction time
//! ([`crate::StepDescriptor`]): a destructive, security-relevant, money-moving, or custom guarded
//! step that omits an explicit [`OnAmbiguous`] is rejected with
//! [`DurableError::AmbiguityPolicyRequired`](crate::DurableError::AmbiguityPolicyRequired), forcing
//! the safety decision to the call site rather than a silent runtime default (FR-DE-09).

/// How a step's side effect behaves under replay.
///
/// This classification is recorded with every step result so the replay cursor can reason about
/// re-execution safety without inspecting the payload.
///
/// # Examples
///
/// ```
/// use zeph_durable::EffectClass;
///
/// // A pure or naturally-idempotent step is safe to skip on replay.
/// assert_eq!(EffectClass::Idempotent.as_str(), "idempotent");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectClass {
    /// The operation is pure or naturally idempotent: a replayed step returns the journaled result
    /// and never invokes the operation closure again (INV-10).
    Idempotent,
    /// The operation tolerates being run more than once. On an ambiguous replay it may be re-run
    /// without correctness loss, accepting at-least-once delivery.
    AtLeastOnce,
    /// The operation must run exactly once. It is fenced by an [`crate::IdempotencyKey`] and an
    /// explicit ambiguity policy; a replayed guarded step never re-fires a committed effect.
    ExactlyOnceGuarded,
}

impl EffectClass {
    /// Return the canonical lower-snake-case string used in the `effect_class` journal column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idempotent => "idempotent",
            Self::AtLeastOnce => "at_least_once",
            Self::ExactlyOnceGuarded => "exactly_once_guarded",
        }
    }

    /// Parse the canonical `effect_class` column string back into an [`EffectClass`].
    ///
    /// Returns `None` for an unrecognized tag so a corrupt journal row fails closed rather than
    /// defaulting to a weaker effect class.
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "idempotent" => Some(Self::Idempotent),
            "at_least_once" => Some(Self::AtLeastOnce),
            "exactly_once_guarded" => Some(Self::ExactlyOnceGuarded),
            _ => None,
        }
    }
}

/// What an [`EffectClass::ExactlyOnceGuarded`] step actually does, refining the ambiguity policy.
///
/// The sub-class drives the construction-time policy rule: a guarded step whose effect is
/// destructive, security-relevant, money-moving, or custom MUST carry an explicit [`OnAmbiguous`]
/// (FR-DE-09); only a cost-bearing / boundary-idempotent effect gets a safe default
/// ([`OnAmbiguous::Skip`]). The sub-class is consumed when the descriptor is built and never stored
/// on the journal row — the persisted classification is the coarser [`EffectClass`].
///
/// # Examples
///
/// ```
/// use zeph_durable::EffectIntentSubClass;
///
/// // A paid LLM call carrying a provider idempotency header is safe to skip on an ambiguous replay.
/// assert!(!EffectIntentSubClass::CostBearingOrBoundaryIdempotent.requires_explicit_policy());
/// // A fund transfer must declare its ambiguity policy explicitly.
/// assert!(EffectIntentSubClass::MoneyMoving.requires_explicit_policy());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectIntentSubClass {
    /// A paid or rate-limited boundary effect that the external service deduplicates by
    /// idempotency key (e.g. a paid LLM call). Default ambiguity policy: [`OnAmbiguous::Skip`].
    CostBearingOrBoundaryIdempotent,
    /// An irreversible mutation (file delete, record drop). Requires an explicit policy.
    Destructive,
    /// A permission or credential mutation. Requires an explicit policy.
    SecurityRelevant,
    /// A financial transfer. Requires an explicit policy.
    MoneyMoving,
    /// A caller-defined effect with no built-in default. Requires an explicit policy.
    Custom,
}

impl EffectIntentSubClass {
    /// Whether a guarded step of this sub-class MUST be given an explicit [`OnAmbiguous`] policy.
    ///
    /// Only [`EffectIntentSubClass::CostBearingOrBoundaryIdempotent`] has a safe default; every
    /// other sub-class forces the decision to the call site.
    #[must_use]
    pub fn requires_explicit_policy(self) -> bool {
        !matches!(self, Self::CostBearingOrBoundaryIdempotent)
    }

    /// Return the canonical lower-snake-case string for diagnostics and audit records.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CostBearingOrBoundaryIdempotent => "cost_bearing_or_boundary_idempotent",
            Self::Destructive => "destructive",
            Self::SecurityRelevant => "security_relevant",
            Self::MoneyMoving => "money_moving",
            Self::Custom => "custom",
        }
    }
}

/// What to do when a guarded step resumes inside the *ambiguous window*.
///
/// The ambiguous window is the gap between committing the `EffectIntent` and committing the
/// `StepResult`: on resume the journal proves the effect was *about to* fire, but not whether it
/// did. The policy resolves that uncertainty. Every resolution emits a mandatory structured audit
/// record (FR-DE-10).
///
/// # Examples
///
/// ```
/// use zeph_durable::OnAmbiguous;
///
/// assert_eq!(OnAmbiguous::Skip.as_str(), "skip");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnAmbiguous {
    /// Assume the effect happened. Safe for cost-bearing / boundary-idempotent effects, where the
    /// external service deduplicates the re-issued operation by its idempotency key, so re-running
    /// the closure cannot double-apply the effect (it is deduplicated at the boundary).
    Skip,
    /// Surface the ambiguity to the operator with [`DurableError::AmbiguousEffect`]. The required
    /// choice for destructive and security-relevant effects, where guessing is unacceptable.
    ///
    /// [`DurableError::AmbiguousEffect`]: crate::DurableError::AmbiguousEffect
    Fail,
    /// Assume the effect did *not* happen and re-run the closure. For effects misclassified as
    /// guarded that are in fact safe to repeat.
    Rerun,
}

impl OnAmbiguous {
    /// Return the canonical lower-snake-case string used in the mandatory audit record.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Fail => "fail",
            Self::Rerun => "rerun",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_class_as_str_is_stable() {
        assert_eq!(EffectClass::Idempotent.as_str(), "idempotent");
        assert_eq!(EffectClass::AtLeastOnce.as_str(), "at_least_once");
        assert_eq!(
            EffectClass::ExactlyOnceGuarded.as_str(),
            "exactly_once_guarded"
        );
    }

    #[test]
    fn only_cost_bearing_subclass_has_a_default_policy() {
        assert!(!EffectIntentSubClass::CostBearingOrBoundaryIdempotent.requires_explicit_policy());
        for sub in [
            EffectIntentSubClass::Destructive,
            EffectIntentSubClass::SecurityRelevant,
            EffectIntentSubClass::MoneyMoving,
            EffectIntentSubClass::Custom,
        ] {
            assert!(
                sub.requires_explicit_policy(),
                "{} must require an explicit ambiguity policy",
                sub.as_str()
            );
        }
    }

    #[test]
    fn subclass_and_policy_strings_are_stable() {
        assert_eq!(
            EffectIntentSubClass::CostBearingOrBoundaryIdempotent.as_str(),
            "cost_bearing_or_boundary_idempotent"
        );
        assert_eq!(EffectIntentSubClass::Destructive.as_str(), "destructive");
        assert_eq!(OnAmbiguous::Skip.as_str(), "skip");
        assert_eq!(OnAmbiguous::Fail.as_str(), "fail");
        assert_eq!(OnAmbiguous::Rerun.as_str(), "rerun");
    }
}
