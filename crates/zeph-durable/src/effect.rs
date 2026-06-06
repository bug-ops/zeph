// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The per-step side-effect contract.
//!
//! [`EffectClass`] declares how a step's side effect behaves under replay. It is the foundation of
//! the exactly-once machinery: the replay cursor uses it to decide whether a journaled result may
//! be returned without re-running the operation.
//!
//! The ambiguity-policy types (`OnAmbiguous`, `EffectIntentSubClass`) and the construction-time
//! policy rule land alongside the durable step primitive in a follow-up issue.

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
}
