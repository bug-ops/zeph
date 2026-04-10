// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Helpers for pinning generation parameters to reproducible values.
//!
//! By default every `bench run` forces `temperature=0.0` on the configured LLM
//! provider so that two runs with identical inputs produce the same output. Pass
//! `--no-deterministic` on the CLI (or `no_deterministic = true` to
//! [`apply_deterministic_overrides`]) to opt out.

use zeph_llm::provider::GenerationOverrides;

/// Build the [`GenerationOverrides`] that pin temperature to `0.0`.
///
/// All other sampling parameters (`top_p`, `top_k`, `frequency_penalty`,
/// `presence_penalty`) are left as `None` so that the provider's own defaults
/// apply.
///
/// # Examples
///
/// ```
/// use zeph_bench::deterministic::deterministic_overrides;
///
/// let overrides = deterministic_overrides();
/// assert_eq!(overrides.temperature, Some(0.0));
/// assert!(overrides.top_p.is_none());
/// ```
#[must_use]
pub fn deterministic_overrides() -> GenerationOverrides {
    GenerationOverrides {
        temperature: Some(0.0),
        top_p: None,
        top_k: None,
        frequency_penalty: None,
        presence_penalty: None,
    }
}

/// Optionally apply deterministic generation overrides to an [`AnyProvider`].
///
/// When `no_deterministic` is `false` (the default for `bench run`), temperature
/// is forced to `0.0` via [`deterministic_overrides`]. When `true` the provider
/// is returned unchanged so the caller's configured temperature is used.
///
/// This function is called by the bench runner after resolving the provider and
/// before constructing the agent.
///
/// # Examples
///
/// ```no_run
/// use zeph_bench::apply_deterministic_overrides;
/// use zeph_llm::{any::AnyProvider, mock::MockProvider};
///
/// let provider = AnyProvider::Mock(MockProvider::with_responses(vec![]));
///
/// // Non-deterministic: provider is returned unchanged.
/// let result = apply_deterministic_overrides(provider, true);
/// assert!(matches!(result, AnyProvider::Mock(_)));
/// ```
///
/// [`AnyProvider`]: zeph_llm::any::AnyProvider
pub fn apply_deterministic_overrides(
    provider: zeph_llm::any::AnyProvider,
    no_deterministic: bool,
) -> zeph_llm::any::AnyProvider {
    if no_deterministic {
        provider
    } else {
        provider.with_generation_overrides(deterministic_overrides())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_overrides_returns_temperature_zero() {
        let overrides = deterministic_overrides();
        assert_eq!(overrides.temperature, Some(0.0));
    }

    #[test]
    fn deterministic_overrides_leaves_other_fields_none() {
        let overrides = deterministic_overrides();
        assert!(overrides.top_p.is_none());
        assert!(overrides.top_k.is_none());
        assert!(overrides.frequency_penalty.is_none());
        assert!(overrides.presence_penalty.is_none());
    }

    #[test]
    fn apply_with_no_deterministic_true_skips_override() {
        // Use Mock provider (zero-network) to verify the skip branch.
        let provider =
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::with_responses(vec![]));
        // When no_deterministic=true, provider is returned without applying overrides.
        // We can't introspect the override directly, but we verify the call doesn't panic
        // and returns an AnyProvider (the mock variant).
        let result = apply_deterministic_overrides(provider, true);
        assert!(matches!(result, zeph_llm::any::AnyProvider::Mock(_)));
    }

    #[test]
    fn apply_with_no_deterministic_false_applies_override() {
        let provider =
            zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::with_responses(vec![]));
        // Mock provider's with_generation_overrides is a no-op but still returns Mock variant.
        let result = apply_deterministic_overrides(provider, false);
        assert!(matches!(result, zeph_llm::any::AnyProvider::Mock(_)));
    }
}
